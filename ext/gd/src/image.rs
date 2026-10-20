//! `GdImage` and `GdFont` (`gd.c`'s object representation), the natives
//! that make them, and what every native reaches an image through: the
//! `gdImagePtr` an object holds and the reports libgd made during a call.

use php_ext::abi::{Instance, ObjectHandlers, OrFalse, Refusals, Union};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{Function, lookup_class, new_object};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, fatal, throw_error};
use php_ext::request::{OwnedStream as _, Request, Stream, Streams as _, Whence};
use php_ext::values::{Emalloc, Owner, PhpObject};

use crate::ffi::{self, Font, Glyphs, Im, Image, Reported};

/// `final class GdImage`, `@strict-properties`, `@not-serializable`: the
/// payload is the image, `None` only on an object no native made.
///
/// ```php
/// try { new GdImage; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $i = imagecreate(1, 1);
/// try { clone $i; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize($i); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { $i->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($i, $i == imagecreate(1, 1));
/// ```
/// ```expect
/// You cannot initialize a GdImage object except through helper functions
/// Trying to clone an uncloneable object of class GdImage
/// Serialization of 'GdImage' is not allowed
/// Cannot create dynamic property GdImage::$x
/// object(GdImage)#1 (0) {
/// }
/// bool(false)
/// ```
#[php_class(
    final,
    not_serializable,
    size = 64, payload = Option<Image>,
    handlers = IMAGE,
    dynamic_properties = forbidden
)]
pub(crate) struct GdImage;

#[php_class]
impl GdImage {}

/// `final class GdFont`, as `GdImage`: the payload is the font
/// `imageloadfont` read.
///
/// ```php
/// try { new GdFont; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// You cannot initialize a GdFont object except through helper functions
/// ```
#[php_class(
    final,
    not_serializable,
    size = 64, payload = Option<Box<Glyphs>>,
    handlers = FONT,
    dynamic_properties = forbidden
)]
pub(crate) struct GdFont;

#[php_class]
impl GdFont {}

/// `php_gd_image_object_handlers`: `std_object_handlers` with
/// `get_constructor` refusing, no `clone_obj` and `compare`
/// `zend_objects_not_comparable`; `free_obj`'s `gdImageDestroy` is the
/// payload's drop.
pub(crate) struct ImageHandlers;

pub(crate) static IMAGE: ImageHandlers = ImageHandlers;

/// What the table refuses: cloning and comparison.
const IMAGE_REFUSALS: Refusals = Refusals::NONE.cloning().comparison();

impl ObjectHandlers for ImageHandlers {
    fn refusals(&self) -> Refusals {
        IMAGE_REFUSALS
    }

    fn get_constructor(
        &self,
        req: &mut Request,
        _object: PhpObject,
    ) -> Result<Option<Function>, Thrown> {
        Err(refuse(req, "GdImage"))
    }
}

/// `php_gd_font_object_handlers`: as an image's, comparable, and its
/// glyphs given back to the heap when it is freed.
pub(crate) struct FontHandlers;

pub(crate) static FONT: FontHandlers = FontHandlers;

/// What the table refuses: cloning.
const FONT_REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for FontHandlers {
    fn refusals(&self) -> Refusals {
        FONT_REFUSALS
    }

    fn get_constructor(
        &self,
        req: &mut Request,
        _object: PhpObject,
    ) -> Result<Option<Function>, Thrown> {
        Err(refuse(req, "GdFont"))
    }

    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let glyphs = object
            .native_mut::<Option<Box<Glyphs>>>(req)
            .and_then(Option::take);
        if let Some(glyphs) = glyphs {
            glyphs.data.free(req);
            glyphs.header.free(req);
        }
    }
}

/// The `get_constructor` of both classes.
fn refuse(req: &mut Request, class: &str) -> Thrown {
    let text = format!("You cannot initialize a {class} object except through helper functions");
    throw_error(req, "Error", &text)
}

/// `php_gd_libgdimageptr_from_zval_p`: the image an object holds, or the
/// `InternalError` for one no native made.
pub(crate) fn im(req: &mut Request, image: Instance<GdImage>) -> Result<Im, Thrown> {
    let held = image.payload_mut(req)?.as_ref().map(Image::im);
    match held {
        Some(im) => Ok(im),
        None => Err(fatal(req, "InternalError: a GdImage no gd native made")),
    }
}

/// `php_gd_assign_libgdimageptr_as_extgdimage`: a new `GdImage` holding
/// `image`, or `false` where libgd made none.
pub(crate) fn wrap(
    req: &mut Request,
    image: Option<Image>,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    let Some(image) = image else {
        return Ok(OrFalse::False);
    };
    let object = object_of(req, b"GdImage")?;
    let made = object.into_instance::<GdImage>();
    *made.payload_mut(req)? = Some(image);
    Ok(OrFalse::Value(made))
}

/// `object_init_ex` of one of the two classes.
fn object_of(req: &mut Request, name: &[u8]) -> Result<Owner<PhpObject>, Thrown> {
    let Some(class) = lookup_class(req, name)? else {
        let text = format!("Class \"{}\" not found", String::from_utf8_lossy(name));
        return Err(throw_error(req, "Error", &text));
    };
    new_object(req, class)
}

/// `php_gd_error_method`'s level for one of libgd's: `GD_DEBUG`,
/// `GD_INFO` and `GD_NOTICE` as `E_NOTICE`, `GD_WARNING` as `E_WARNING`,
/// and the rest, `GD_ERROR` and the three past it, as `E_ERROR`.
const fn level(gd: i32) -> Level {
    match gd {
        5..=7 => Level::Notice,
        4 => Level::Warning,
        _ => Level::Error,
    }
}

/// `php_gd_error_method` for each message libgd raised during a call,
/// through `php_verror` as the native's docref. An `E_ERROR` ends the
/// request where php's bails out of libgd: what came before it stands,
/// nothing after it is raised, and the native answers through the
/// `Thrown`. No snippet reaches one through the keg's libgd 2.3.3, whose
/// `GD_ERROR`s are its XPM reader, which php declares no native over
/// without XPM support, and the PNG and JPEG handlers' unrecoverable
/// jumps, so the level's mapping is pinned by the crate's test alone.
/// The text is libgd's bytes, raised as php's `%s` prints them.
pub(crate) fn raise(req: &mut Request, reported: Reported) -> Result<(), Thrown> {
    for (gd, text) in reported {
        match level(gd) {
            Level::Error => {
                let function = req.frames().active_function_name();
                let message = [function.as_bytes(), b"(): ", &text].concat();
                return Err(fatal(req, &message));
            }
            level => docref(req, level, &text),
        }
    }
    Ok(())
}

/// `f` over libgd with its reports raised once it returns; the `Err` is
/// the `E_ERROR` that ended the request.
pub(crate) fn gd<R>(req: &mut Request, f: impl FnOnce() -> R) -> Result<R, Thrown> {
    let (answer, reported) = ffi::capture(f);
    raise(req, reported)?;
    Ok(answer)
}

/// `overflow2` (`gd_compat.c`): whether `a * b` cannot be an `int`
/// allocation, with the warning php raises for it.
pub(crate) fn overflow2(req: &mut Request, a: i32, b: i32) -> bool {
    if a <= 0 || b <= 0 {
        docref(
            req,
            Level::Warning,
            "One parameter to a memory allocation multiplication is negative or zero, failing operation gracefully\n",
        );
        return true;
    }
    if a > i32::MAX / b {
        docref(
            req,
            Level::Warning,
            "Product of memory allocation multiplication would exceed INT_MAX, failing operation gracefully\n",
        );
        return true;
    }
    false
}

/// `php_find_gd_font`: the font a `GdFont` holds, or one of libgd's five
/// by number, the nearest for a number past them.
pub(crate) fn with_font<R>(
    req: &mut Request,
    font: Union<Instance<GdFont>, i64>,
    f: impl FnOnce(&mut Request, &Font) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    match font {
        Union::B(n) => f(req, &Font::Builtin(n)),
        Union::A(object) => {
            let glyphs = object.payload_mut(req)?.take();
            let Some(glyphs) = glyphs else {
                return Err(fatal(
                    req,
                    "InternalError: a GdFont imageloadfont() did not read",
                ));
            };
            let answer = f(req, &Font::Loaded(&glyphs));
            *object.payload_mut(req)? = Some(glyphs);
            answer
        }
    }
}

/// `imagecreatetruecolor(int $width, int $height): GdImage|false`.
///
/// ```php
/// $i = imagecreatetruecolor(3, 2);
/// var_dump(imagesx($i), imagesy($i), imageistruecolor($i), imagecolorat($i, 0, 0));
/// try { imagecreatetruecolor(0, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imagecreatetruecolor(1, PHP_INT_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// int(2)
/// bool(true)
/// int(0)
/// imagecreatetruecolor(): Argument #1 ($width) must be greater than 0
/// imagecreatetruecolor(): Argument #2 ($height) must be greater than 0
/// ```
#[php_function]
fn imagecreatetruecolor(
    req: &mut Request,
    width: i64,
    height: i64,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    let (w, h) = sizes(req, width, height)?;
    let image = gd(req, || Image::create_true_color(w, h))?;
    wrap(req, image)
}

/// The two sizes `imagecreate` and `imagecreatetruecolor` take, each
/// above zero and below `INT_MAX`.
fn sizes(req: &mut Request, width: i64, height: i64) -> Result<(i32, i32), Thrown> {
    let fits = |n: i64| n > 0 && n < i64::from(i32::MAX);
    if !fits(width) {
        return Err(arg_value_error(req, 1, "be greater than 0"));
    }
    if !fits(height) {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    }
    Ok((int(width), int(height)))
}

/// A `zend_long` as C's `(int)` cast reads it, the low 32 bits.
#[allow(clippy::cast_possible_truncation)]
pub(crate) const fn int(n: i64) -> i32 {
    n as i32
}

/// `imageistruecolor(GdImage $image): bool`.
///
/// ```php
/// var_dump(imageistruecolor(imagecreate(1, 1)), imageistruecolor(imagecreatetruecolor(1, 1)));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn imageistruecolor(req: &mut Request, image: Instance<GdImage>) -> Result<bool, Thrown> {
    Ok(im(req, image)?.true_color())
}

/// `imagecreate(int $width, int $height): GdImage|false`: a palette
/// image.
///
/// ```php
/// $i = imagecreate(4, 5);
/// var_dump(imagesx($i), imagesy($i), imagecolorstotal($i));
/// try { imagecreate(-1, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(4)
/// int(5)
/// int(0)
/// imagecreate(): Argument #1 ($width) must be greater than 0
/// ```
#[php_function]
fn imagecreate(
    req: &mut Request,
    width: i64,
    height: i64,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    let (w, h) = sizes(req, width, height)?;
    let image = gd(req, || Image::create(w, h))?;
    wrap(req, image)
}

/// `FLIPWORD`: a header word read in the other byte order.
const fn flip(word: i32) -> i32 {
    word.swap_bytes()
}

/// `imageloadfont(string $filename): GdFont|false`: php's binary font
/// dump, a header of four `int`s in either byte order, then one byte per
/// pixel of every glyph, its length held to the file's.
///
/// ```php
/// $f = __DIR__ . '/w.gdf';
/// file_put_contents($f, pack('V4', 1, 65, 2, 2) . "\1\0\0\1");
/// $font = imageloadfont($f);
/// var_dump($font, imagefontwidth($font), imagefontheight($font));
/// file_put_contents($f, pack('V4', 1, 65, 2, 2) . "\1");
/// var_dump(imageloadfont($f));
/// file_put_contents($f, "abc");
/// var_dump(imageloadfont($f));
/// unlink($f);
/// ```
/// ```expect
/// object(GdFont)#1 (0) {
/// }
/// int(2)
/// int(2)
///
/// Warning: imageloadfont(): Product of memory allocation multiplication would exceed INT_MAX, failing operation gracefully
///  in %s on line 7
///
/// Warning: imageloadfont(): Error reading font, invalid font header in %s on line 7
/// bool(false)
///
/// Warning: imageloadfont(): End of file while reading header in %s on line 9
/// bool(false)
/// ```
#[php_function]
fn imageloadfont(
    req: &mut Request,
    filename: php_ext::abi::Path,
) -> Result<OrFalse<Owner<Instance<GdFont>>>, Thrown> {
    let Some(stream) = req.open_stream(filename.0.as_bytes(), b"rb", false)? else {
        return Ok(OrFalse::False);
    };
    let header = Emalloc::new(req, 24)?;
    let read = load(req, *stream);
    stream.free(req);
    let glyphs = match read? {
        Ok((nchars, offset, w, h, data)) => Glyphs {
            nchars,
            offset,
            w,
            h,
            header,
            data,
        },
        Err(text) => {
            header.free(req);
            if let Some(text) = text {
                docref(req, Level::Warning, text);
            }
            return Ok(OrFalse::False);
        }
    };
    let object = object_of(req, b"GdFont")?;
    let font = object.into_instance::<GdFont>();
    *font.payload_mut(req)? = Some(Box::new(glyphs));
    Ok(OrFalse::Value(font))
}

/// A font's header and glyphs, or the warning that ends the read.
type Loaded = Result<(i32, i32, i32, i32, Emalloc), Option<&'static str>>;

/// The body of `imageloadfont` between the open and the close.
fn load(req: &mut Request, stream: Stream) -> Result<Loaded, Thrown> {
    let mut header = [0u8; 16];
    if !read_full(req, stream, &mut header) {
        return Ok(Err(Some(if stream.eof(req) {
            "End of file while reading header"
        } else {
            "Error while reading header"
        })));
    }
    let word = |i: usize| ffi::native_int([header[i], header[i + 1], header[i + 2], header[i + 3]]);
    let (mut nchars, offset, mut w, mut h) = (word(0), word(4), word(8), word(12));
    let at = stream.tell(req);
    stream.seek(req, 0, Whence::End);
    let check = stream.tell(req) - 16;
    stream.seek(req, at, Whence::Set);
    if overflow2(req, nchars, h) || overflow2(req, nchars.wrapping_mul(h), w) {
        return Ok(Err(Some("Error reading font, invalid font header")));
    }
    let mut size = w.wrapping_mul(h).wrapping_mul(nchars);
    if i64::from(size) != check {
        (w, h, nchars) = (flip(w), flip(h), flip(nchars));
        if overflow2(req, nchars, h) || overflow2(req, nchars.wrapping_mul(h), w) {
            return Ok(Err(Some("Error reading font, invalid font header")));
        }
        size = w.wrapping_mul(h).wrapping_mul(nchars);
    }
    if i64::from(size) != check {
        return Ok(Err(Some("Error reading font")));
    }
    let mut data = Emalloc::new(req, usize::try_from(size).unwrap_or(0))?;
    if !read_full(req, stream, data.bytes_mut()) {
        let text = if stream.eof(req) {
            "End of file while reading body"
        } else {
            "Error while reading body"
        };
        data.free(req);
        return Ok(Err(Some(text)));
    }
    Ok(Ok((nchars, offset, w, h, data)))
}

/// php's loop of `php_stream_read` until `buf` is full: whether it was.
fn read_full(req: &mut Request, stream: Stream, buf: &mut [u8]) -> bool {
    let mut filled = 0;
    while filled < buf.len() {
        match stream.read_into(req, &mut buf[filled..]) {
            Some(n) if n > 0 => filled += n,
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{Level, level};

    /// `php_gd_error_method`'s switch over `gd_errors.h`'s levels,
    /// `GD_EMERG` 0 to `GD_DEBUG` 7.
    #[test]
    fn libgd_levels_are_php_levels() {
        let levels: Vec<Level> = (0..=7).map(level).collect();
        assert_eq!(
            levels,
            [
                Level::Error,
                Level::Error,
                Level::Error,
                Level::Error,
                Level::Warning,
                Level::Notice,
                Level::Notice,
                Level::Notice,
            ]
        );
    }
}
