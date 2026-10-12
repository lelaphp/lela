//! The binding to libgd (design §7.2, revision 75; `unsafe.homes`'
//! `ext/*/src/ffi/`): the keg's libgd 2.3.3, the library the oracle
//! links, which `build.rs` finds through `pkg-config gdlib`, and the one
//! home of this crate's `unsafe`. The declarations are `gd.h`'s and
//! `gd_io.h`'s, written here, no bindgen; `error.c` is the error method.
//!
//! php sets no allocator on an external libgd (`gd.c` calls no
//! `gdSetMemory*`), so an image's pixels are the process's and
//! `memory_get_usage()` counts the object alone, as the oracle's does.
//!
//! An [`Image`] owns one `gdImagePtr`, destroyed when it drops, as
//! `php_gd_image_object_free` destroys it; an [`Im`] names one for the
//! length of a call, which the object holding it outlives.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]
// libgd's own names for its numbers: `r`, `g`, `b`, `a`, `cx`, `w`.
#![allow(clippy::many_single_char_names)]

use std::ffi::{CStr, CString, c_char, c_double, c_float, c_int, c_long, c_uint, c_void};
use std::ptr::{self, NonNull};

/// `gdMaxColors`.
const MAX_COLORS: usize = 256;

/// `gdImage`, the fields php reads and writes through the pointer, in
/// `gd.h`'s order up to `interpolation_id`; libgd allocates it, and the
/// rest of it is never named here.
#[repr(C)]
struct GdImage {
    pixels: *mut *mut u8,
    sx: c_int,
    sy: c_int,
    colors_total: c_int,
    red: [c_int; MAX_COLORS],
    green: [c_int; MAX_COLORS],
    blue: [c_int; MAX_COLORS],
    open: [c_int; MAX_COLORS],
    transparent: c_int,
    poly_ints: *mut c_int,
    poly_allocated: c_int,
    brush: *mut GdImage,
    tile: *mut GdImage,
    brush_color_map: [c_int; MAX_COLORS],
    tile_color_map: [c_int; MAX_COLORS],
    style_length: c_int,
    style_pos: c_int,
    style: *mut c_int,
    interlace: c_int,
    thick: c_int,
    alpha: [c_int; MAX_COLORS],
    true_color: c_int,
    tpixels: *mut *mut c_int,
    alpha_blending_flag: c_int,
    save_alpha_flag: c_int,
    aa: c_int,
    aa_color: c_int,
    aa_dont_blend: c_int,
    cx1: c_int,
    cy1: c_int,
    cx2: c_int,
    cy2: c_int,
    res_x: c_uint,
    res_y: c_uint,
    palette_quantization_method: c_int,
    palette_quantization_speed: c_int,
    palette_quantization_min_quality: c_int,
    palette_quantization_max_quality: c_int,
    interpolation_id: c_int,
}

/// `gdFont`.
#[repr(C)]
#[derive(Clone, Copy)]
struct GdFont {
    nchars: c_int,
    offset: c_int,
    w: c_int,
    h: c_int,
    data: *mut c_char,
}

/// `gdIOCtx` (`gd_io.h`): what libgd reads through and writes through.
/// The encoders' is [`writing`]'s, built here as `create_output_context`
/// builds php's.
#[repr(C)]
struct IoCtx {
    get_c: Option<unsafe extern "C" fn(*mut IoCtx) -> c_int>,
    get_buf: Option<unsafe extern "C" fn(*mut IoCtx, *mut c_void, c_int) -> c_int>,
    put_c: Option<unsafe extern "C" fn(*mut IoCtx, c_int)>,
    put_buf: Option<unsafe extern "C" fn(*mut IoCtx, *const c_void, c_int) -> c_int>,
    seek: Option<unsafe extern "C" fn(*mut IoCtx, c_int) -> c_int>,
    tell: Option<unsafe extern "C" fn(*mut IoCtx) -> c_long>,
    gd_free: Option<unsafe extern "C" fn(*mut IoCtx)>,
    data: *mut c_void,
}

/// `sizeof(gdIOCtx)`, what php's `ecalloc` of one counts.
pub(crate) const CTX_SIZE: usize = std::mem::size_of::<IoCtx>();

/// `gdPoint`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Point {
    pub(crate) x: c_int,
    pub(crate) y: c_int,
}

/// `gdRect`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Rect {
    pub(crate) x: c_int,
    pub(crate) y: c_int,
    pub(crate) width: c_int,
    pub(crate) height: c_int,
}

/// `gdFTStringExtra`.
#[repr(C)]
struct FtExtra {
    flags: c_int,
    linespacing: c_double,
    charmap: c_int,
    hdpi: c_int,
    vdpi: c_int,
    xshow: *mut c_char,
    fontpath: *mut c_char,
}

type Ptr = *mut GdImage;
type Size = *mut c_int;

unsafe extern "C" {
    fn php_gd_capture(sink: *mut c_void) -> *mut c_void;
    fn gdVersionString() -> *const c_char;
    fn gdFree(m: *mut c_void);
    fn gdFontCacheShutdown();
    fn gdFontGetTiny() -> *mut GdFont;
    fn gdFontGetSmall() -> *mut GdFont;
    fn gdFontGetMediumBold() -> *mut GdFont;
    fn gdFontGetLarge() -> *mut GdFont;
    fn gdFontGetGiant() -> *mut GdFont;
    fn gdImageCreate(sx: c_int, sy: c_int) -> Ptr;
    fn gdImageCreateTrueColor(sx: c_int, sy: c_int) -> Ptr;
    fn gdImageDestroy(im: Ptr);
    fn gdImageClone(src: Ptr) -> Ptr;
    fn gdImageCreateFromPngPtr(size: c_int, data: *mut c_void) -> Ptr;
    fn gdImageCreateFromGifPtr(size: c_int, data: *mut c_void) -> Ptr;
    fn gdImageCreateFromWBMPPtr(size: c_int, data: *mut c_void) -> Ptr;
    fn gdImageCreateFromJpegPtrEx(size: c_int, data: *mut c_void, ignore: c_int) -> Ptr;
    fn gdImageCreateFromWebpPtr(size: c_int, data: *mut c_void) -> Ptr;
    fn gdImageCreateFromAvifPtr(size: c_int, data: *mut c_void) -> Ptr;
    fn gdImageCreateFromTgaPtr(size: c_int, data: *mut c_void) -> Ptr;
    fn gdImageCreateFromBmpPtr(size: c_int, data: *mut c_void) -> Ptr;
    fn gdImageCreateFromGdPtr(size: c_int, data: *mut c_void) -> Ptr;
    fn gdImageCreateFromGd2Ptr(size: c_int, data: *mut c_void) -> Ptr;
    fn gdImageCreateFromGd2PartPtr(
        size: c_int,
        data: *mut c_void,
        x: c_int,
        y: c_int,
        w: c_int,
        h: c_int,
    ) -> Ptr;
    fn gdImageCreateFromXbm(file: *mut libc_file::File) -> Ptr;
    fn gdImageGifCtx(im: Ptr, out: *mut IoCtx);
    fn gdImagePngCtxEx(im: Ptr, out: *mut IoCtx, level: c_int);
    fn gdImageJpegCtx(im: Ptr, out: *mut IoCtx, quality: c_int);
    fn gdImageWebpCtx(im: Ptr, out: *mut IoCtx, quality: c_int);
    fn gdImageAvifCtx(im: Ptr, out: *mut IoCtx, quality: c_int, speed: c_int);
    fn gdImageWBMPCtx(im: Ptr, fg: c_int, out: *mut IoCtx);
    fn gdImageBmpCtx(im: Ptr, out: *mut IoCtx, compression: c_int);
    fn gdImageGdPtr(im: Ptr, size: Size) -> *mut c_void;
    fn gdImageGd2Ptr(im: Ptr, cs: c_int, fmt: c_int, size: Size) -> *mut c_void;
    fn gdImageXbmCtx(im: Ptr, name: *mut c_char, fg: c_int, out: *mut IoCtx);
    fn gdImageSetStyle(im: Ptr, style: *mut c_int, n: c_int);
    fn gdImageTrueColorToPalette(im: Ptr, dither: c_int, colors: c_int) -> c_int;
    fn gdImagePaletteToTrueColor(im: Ptr) -> c_int;
    fn gdImageColorMatch(im1: Ptr, im2: Ptr) -> c_int;
    fn gdImageSetThickness(im: Ptr, thick: c_int);
    fn gdImageFilledEllipse(im: Ptr, cx: c_int, cy: c_int, w: c_int, h: c_int, c: c_int);
    fn gdImageFilledArc(
        im: Ptr,
        cx: c_int,
        cy: c_int,
        w: c_int,
        h: c_int,
        s: c_int,
        e: c_int,
        c: c_int,
        style: c_int,
    );
    fn gdImageArc(im: Ptr, cx: c_int, cy: c_int, w: c_int, h: c_int, s: c_int, e: c_int, c: c_int);
    fn gdImageEllipse(im: Ptr, cx: c_int, cy: c_int, w: c_int, h: c_int, c: c_int);
    fn gdImageAlphaBlending(im: Ptr, on: c_int);
    fn gdImageSaveAlpha(im: Ptr, on: c_int);
    fn gdImageColorAllocate(im: Ptr, r: c_int, g: c_int, b: c_int) -> c_int;
    fn gdImageColorAllocateAlpha(im: Ptr, r: c_int, g: c_int, b: c_int, a: c_int) -> c_int;
    fn gdImageColorResolveAlpha(im: Ptr, r: c_int, g: c_int, b: c_int, a: c_int) -> c_int;
    fn gdImageColorClosestAlpha(im: Ptr, r: c_int, g: c_int, b: c_int, a: c_int) -> c_int;
    fn gdImageColorExactAlpha(im: Ptr, r: c_int, g: c_int, b: c_int, a: c_int) -> c_int;
    fn gdImageColorClosestHWB(im: Ptr, r: c_int, g: c_int, b: c_int) -> c_int;
    fn gdImageColorDeallocate(im: Ptr, c: c_int);
    fn gdImageColorTransparent(im: Ptr, c: c_int);
    fn gdImagePaletteCopy(dst: Ptr, src: Ptr);
    fn gdImageCopy(
        dst: Ptr,
        src: Ptr,
        dx: c_int,
        dy: c_int,
        sx: c_int,
        sy: c_int,
        w: c_int,
        h: c_int,
    );
    fn gdImageCopyMerge(
        dst: Ptr,
        src: Ptr,
        dx: c_int,
        dy: c_int,
        sx: c_int,
        sy: c_int,
        w: c_int,
        h: c_int,
        pct: c_int,
    );
    fn gdImageCopyMergeGray(
        dst: Ptr,
        src: Ptr,
        dx: c_int,
        dy: c_int,
        sx: c_int,
        sy: c_int,
        w: c_int,
        h: c_int,
        pct: c_int,
    );
    fn gdImageCopyResized(
        dst: Ptr,
        src: Ptr,
        dx: c_int,
        dy: c_int,
        sx: c_int,
        sy: c_int,
        dw: c_int,
        dh: c_int,
        sw: c_int,
        sh: c_int,
    );
    fn gdImageCopyResampled(
        dst: Ptr,
        src: Ptr,
        dx: c_int,
        dy: c_int,
        sx: c_int,
        sy: c_int,
        dw: c_int,
        dh: c_int,
        sw: c_int,
        sh: c_int,
    );
    fn gdImageRotateInterpolated(src: Ptr, angle: c_float, bg: c_int) -> Ptr;
    fn gdImageSetTile(im: Ptr, tile: Ptr);
    fn gdImageSetBrush(im: Ptr, brush: Ptr);
    fn gdImageGetPixel(im: Ptr, x: c_int, y: c_int) -> c_int;
    fn gdImageSetPixel(im: Ptr, x: c_int, y: c_int, c: c_int);
    fn gdImageBoundsSafe(im: Ptr, x: c_int, y: c_int) -> c_int;
    fn gdImageSetAntiAliased(im: Ptr, c: c_int);
    fn gdImageLine(im: Ptr, x1: c_int, y1: c_int, x2: c_int, y2: c_int, c: c_int);
    fn gdImageDashedLine(im: Ptr, x1: c_int, y1: c_int, x2: c_int, y2: c_int, c: c_int);
    fn gdImageRectangle(im: Ptr, x1: c_int, y1: c_int, x2: c_int, y2: c_int, c: c_int);
    fn gdImageFilledRectangle(im: Ptr, x1: c_int, y1: c_int, x2: c_int, y2: c_int, c: c_int);
    fn gdImageFillToBorder(im: Ptr, x: c_int, y: c_int, border: c_int, c: c_int);
    fn gdImageFill(im: Ptr, x: c_int, y: c_int, c: c_int);
    fn gdImageInterlace(im: Ptr, on: c_int);
    fn gdImageOpenPolygon(im: Ptr, p: *mut Point, n: c_int, c: c_int);
    fn gdImagePolygon(im: Ptr, p: *mut Point, n: c_int, c: c_int);
    fn gdImageFilledPolygon(im: Ptr, p: *mut Point, n: c_int, c: c_int);
    fn gdImageChar(im: Ptr, f: *mut GdFont, x: c_int, y: c_int, c: c_int, color: c_int);
    fn gdImageCharUp(im: Ptr, f: *mut GdFont, x: c_int, y: c_int, c: c_int, color: c_int);
    fn gdImageSetClip(im: Ptr, x1: c_int, y1: c_int, x2: c_int, y2: c_int);
    fn gdImageGetClip(im: Ptr, x1: *mut c_int, y1: *mut c_int, x2: *mut c_int, y2: *mut c_int);
    fn gdImageStringFTEx(
        im: Ptr,
        brect: *mut c_int,
        fg: c_int,
        font: *const c_char,
        ptsize: c_double,
        angle: c_double,
        x: c_int,
        y: c_int,
        text: *const c_char,
        extra: *mut FtExtra,
    ) -> *mut c_char;
    fn gdImageStringFT(
        im: Ptr,
        brect: *mut c_int,
        fg: c_int,
        font: *const c_char,
        ptsize: c_double,
        angle: c_double,
        x: c_int,
        y: c_int,
        text: *const c_char,
    ) -> *mut c_char;
    fn gdImageNegate(im: Ptr) -> c_int;
    fn gdImageGrayScale(im: Ptr) -> c_int;
    fn gdImageBrightness(im: Ptr, brightness: c_int) -> c_int;
    fn gdImageContrast(im: Ptr, contrast: c_double) -> c_int;
    fn gdImageColor(im: Ptr, r: c_int, g: c_int, b: c_int, a: c_int) -> c_int;
    fn gdImageEdgeDetectQuick(im: Ptr) -> c_int;
    fn gdImageEmboss(im: Ptr) -> c_int;
    fn gdImageGaussianBlur(im: Ptr) -> c_int;
    fn gdImageSelectiveBlur(im: Ptr) -> c_int;
    fn gdImageMeanRemoval(im: Ptr) -> c_int;
    fn gdImageSmooth(im: Ptr, weight: c_float) -> c_int;
    fn gdImagePixelate(im: Ptr, block: c_int, mode: c_uint) -> c_int;
    fn gdImageScatter(im: Ptr, sub: c_int, plus: c_int) -> c_int;
    fn gdImageScatterColor(
        im: Ptr,
        sub: c_int,
        plus: c_int,
        colors: *mut c_int,
        n: c_uint,
    ) -> c_int;
    fn gdImageConvolution(
        im: Ptr,
        filter: *mut [c_float; 3],
        div: c_float,
        offset: c_float,
    ) -> c_int;
    fn gdImageFlipVertical(im: Ptr);
    fn gdImageFlipHorizontal(im: Ptr);
    fn gdImageFlipBoth(im: Ptr);
    fn gdImageCrop(im: Ptr, rect: *const Rect) -> Ptr;
    fn gdImageCropAuto(im: Ptr, mode: c_uint) -> Ptr;
    fn gdImageCropThreshold(im: Ptr, color: c_uint, threshold: c_float) -> Ptr;
    fn gdImageSetInterpolationMethod(im: Ptr, id: c_int) -> c_int;
    fn gdImageGetInterpolationMethod(im: Ptr) -> c_int;
    fn gdImageScale(im: Ptr, w: c_uint, h: c_uint) -> Ptr;
    fn gdTransformAffineGetImage(
        dst: *mut Ptr,
        src: Ptr,
        area: *mut Rect,
        affine: *const c_double,
    ) -> c_int;
    fn gdAffineTranslate(dst: *mut c_double, x: c_double, y: c_double) -> c_int;
    fn gdAffineScale(dst: *mut c_double, x: c_double, y: c_double) -> c_int;
    fn gdAffineRotate(dst: *mut c_double, angle: c_double) -> c_int;
    fn gdAffineShearHorizontal(dst: *mut c_double, angle: c_double) -> c_int;
    fn gdAffineShearVertical(dst: *mut c_double, angle: c_double) -> c_int;
    fn gdAffineConcat(dst: *mut c_double, m1: *const c_double, m2: *const c_double) -> c_int;
    fn gdImageSetResolution(im: Ptr, x: c_uint, y: c_uint);
}

/// The C library's `FILE` and `fmemopen`, which `gdImageCreateFromXbm`
/// alone needs: libgd reads XBM from a `FILE *` and through no `gdIOCtx`.
mod libc_file {
    use std::ffi::{c_char, c_int, c_void};

    /// `FILE`, opaque.
    #[repr(C)]
    pub(super) struct File {
        _opaque: [u8; 0],
    }

    unsafe extern "C" {
        pub(super) fn fmemopen(buf: *mut c_void, size: usize, mode: *const c_char) -> *mut File;
        pub(super) fn fclose(file: *mut File) -> c_int;
    }
}

// ------------------------------------------------------------ errors

/// What libgd reported through `php_gd_error_method` during one capture,
/// in order: its level, `GD_WARNING` and kin, and the printed text.
pub(crate) type Reported = Vec<(i32, Vec<u8>)>;

/// `php_gd_error_method` past `error.c`: one message joined to the
/// capture running on this thread.
#[unsafe(no_mangle)]
extern "C" fn php_gd_error_text(sink: *mut c_void, level: c_int, text: *const c_char, len: usize) {
    // SAFETY: `error.c` hands `len` bytes of a live string.
    let bytes = unsafe { std::slice::from_raw_parts(text.cast::<u8>(), len) };
    let sink = sink.cast::<Reported>();
    // SAFETY: the sink `capture` set on this thread is the live vector
    // of its frame, which nothing else borrows while libgd runs.
    let sink = unsafe { &mut *sink };
    sink.push((level, bytes.to_vec()));
}

/// `f`, with what libgd reports during it collected rather than lost.
pub(crate) fn capture<R>(f: impl FnOnce() -> R) -> (R, Reported) {
    capturing(|_| f())
}

/// [`capture`], `f` handed the vector the reports land in, which it
/// reads through that pointer alone while libgd is not reporting.
fn capturing<R>(f: impl FnOnce(*mut Reported) -> R) -> (R, Reported) {
    let mut reported: Reported = Vec::new();
    let sink = &raw mut reported;
    // SAFETY: the sink is this frame's vector, live until the call below
    // puts the saved one back.
    let saved = unsafe { php_gd_capture(sink.cast::<c_void>()) };
    let answer = f(sink);
    // SAFETY: the sink this thread had, set again.
    unsafe { php_gd_capture(saved) };
    (answer, reported)
}

// ------------------------------------------------------------ output

/// Where an encoder's bytes go as libgd writes them: handed what libgd
/// reported since the last write and the bytes, it answers how many the
/// sink took, `-1` for a write that failed, as `php_stream_write` does.
pub(crate) type Put<'a> = dyn FnMut(Reported, &[u8]) -> c_int + 'a;

/// What [`writing`]'s `gdIOCtx` points its `data` at.
struct Out<'p, 'a> {
    reported: *mut Reported,
    put: &'p mut Put<'a>,
}

/// `create_output_context`'s `gdIOCtx` over `put`, a stack value: `putC`
/// and `putBuf` alone, as php's `ecalloc` leaves the rest `NULL`, so
/// libgd seeks none (a compressed BMP goes through its temporary file,
/// as it does for php) and `gd_free` is the caller's. `f` is handed it
/// with the reports captured; what libgd reported after its last write
/// is answered.
fn writing(put: &mut Put<'_>, f: impl FnOnce(*mut IoCtx)) -> Reported {
    let ((), left) = capturing(|reported| {
        let mut out = Out { reported, put };
        let mut ctx = IoCtx {
            get_c: None,
            get_buf: None,
            put_c: Some(put_c),
            put_buf: Some(put_buf),
            seek: None,
            tell: None,
            gd_free: None,
            data: (&raw mut out).cast::<c_void>(),
        };
        f(&raw mut ctx);
    });
    left
}

/// `_php_image_output_putc` and `_php_image_stream_putc`: one byte, the
/// `int` libgd passes cut to its low eight bits as php's cast cuts it.
unsafe extern "C" fn put_c(ctx: *mut IoCtx, c: c_int) {
    let byte = c.to_le_bytes()[0];
    // SAFETY: libgd hands back the `gdIOCtx` `writing` made.
    unsafe { put(ctx, &[byte]) };
}

/// `_php_image_output_putbuf` and `_php_image_stream_putbuf`.
unsafe extern "C" fn put_buf(ctx: *mut IoCtx, buf: *const c_void, len: c_int) -> c_int {
    let len = usize::try_from(len).unwrap_or(0);
    if buf.is_null() || len == 0 {
        // SAFETY: as `put_c`.
        return unsafe { put(ctx, &[]) };
    }
    // SAFETY: libgd hands `len` bytes it holds for the call.
    let bytes = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len) };
    // SAFETY: as `put_c`.
    unsafe { put(ctx, bytes) }
}

/// The bytes to the sink, after what libgd reported before them.
///
/// # Safety
///
/// `ctx` is the `gdIOCtx` [`writing`] made, live for the encode.
unsafe fn put(ctx: *mut IoCtx, bytes: &[u8]) -> c_int {
    // SAFETY: the caller's `gdIOCtx`, its `data` the `Out` of `writing`.
    let data = unsafe { (*ctx).data };
    // SAFETY: the `Out` of `writing`'s frame, which nothing else reaches
    // while libgd writes.
    let out = unsafe { &mut *data.cast::<Out<'_, '_>>() };
    // SAFETY: the capture's vector, which libgd is not reporting into
    // while it writes.
    let pending = std::mem::take(unsafe { &mut *out.reported });
    (out.put)(pending, bytes)
}

// ------------------------------------------------------------ library

/// The bytes of a C string, empty for NULL.
fn text(at: *const c_char) -> Vec<u8> {
    if at.is_null() {
        return Vec::new();
    }
    // SAFETY: libgd answers a NUL-terminated string that lives at least
    // as long as this copy.
    let text = unsafe { CStr::from_ptr(at) };
    text.to_bytes().to_vec()
}

/// `bytes` as C reads them, ending at the first NUL.
pub(crate) fn c_string(bytes: &[u8]) -> CString {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    CString::new(&bytes[..end]).unwrap_or_default()
}

/// `gdVersionString()`, `2.3.3` for the keg.
pub(crate) fn version() -> String {
    // SAFETY: libgd's static string.
    let at = unsafe { gdVersionString() };
    String::from_utf8_lossy(&text(at)).into_owned()
}

/// `gdFontCacheShutdown()`, what `PHP_RSHUTDOWN_FUNCTION(gd)` calls.
pub(crate) fn font_cache_shutdown() {
    // SAFETY: libgd's own cache, which it guards with its own mutex.
    unsafe { gdFontCacheShutdown() };
}

/// A block libgd allocated, copied out and freed with `gdFree`.
fn taken(at: *mut c_void, size: c_int) -> Option<Vec<u8>> {
    if at.is_null() {
        return None;
    }
    let len = usize::try_from(size).unwrap_or(0);
    // SAFETY: libgd answered `size` bytes at `at`.
    let bytes = unsafe { std::slice::from_raw_parts(at.cast::<u8>(), len) }.to_vec();
    // SAFETY: the block is libgd's, freed once, after the copy.
    unsafe { gdFree(at) };
    Some(bytes)
}

// ------------------------------------------------------------ images

/// One `gdImagePtr`, owned: destroyed when it drops.
pub(crate) struct Image(NonNull<GdImage>);

// SAFETY: the image is reached only through the object that owns it,
// which one request serves on one thread at a time (ADR-0005); libgd
// keeps no per-thread state for an image.
unsafe impl Send for Image {}

impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: the image is this value's alone, destroyed once.
        unsafe { gdImageDestroy(self.0.as_ptr()) };
    }
}

impl Image {
    fn of(at: Ptr) -> Option<Image> {
        NonNull::new(at).map(Image)
    }

    /// The image to hand libgd for the length of a call.
    pub(crate) fn im(&self) -> Im {
        Im(self.0)
    }

    /// `gdImageCreate`.
    pub(crate) fn create(sx: i32, sy: i32) -> Option<Image> {
        // SAFETY: libgd checks the sizes and answers NULL on failure.
        Image::of(unsafe { gdImageCreate(sx, sy) })
    }

    /// `gdImageCreateTrueColor`.
    pub(crate) fn create_true_color(sx: i32, sy: i32) -> Option<Image> {
        // SAFETY: as `create`.
        Image::of(unsafe { gdImageCreateTrueColor(sx, sy) })
    }

    /// The image `bytes` hold in `format`, read as libgd's `Ptr` readers
    /// read a dynamic `gdIOCtx` over them; `None` where libgd answered
    /// NULL.
    pub(crate) fn decode(format: Format, bytes: &[u8]) -> Option<Image> {
        let mut copy = bytes.to_vec();
        let size = c_int::try_from(copy.len()).ok()?;
        let data = copy.as_mut_ptr().cast::<c_void>();
        let read = match format {
            Format::Png => gdImageCreateFromPngPtr,
            Format::Gif => gdImageCreateFromGifPtr,
            Format::Wbmp => gdImageCreateFromWBMPPtr,
            Format::Jpeg(ignore) => {
                // SAFETY: `size` bytes at `data`, which libgd only reads.
                return Image::of(unsafe { gdImageCreateFromJpegPtrEx(size, data, ignore.into()) });
            }
            Format::Webp => gdImageCreateFromWebpPtr,
            Format::Avif => gdImageCreateFromAvifPtr,
            Format::Tga => gdImageCreateFromTgaPtr,
            Format::Bmp => gdImageCreateFromBmpPtr,
            Format::Gd => gdImageCreateFromGdPtr,
            Format::Gd2 => gdImageCreateFromGd2Ptr,
            Format::Gd2Part(rect) => {
                // SAFETY: as above.
                let at = unsafe {
                    gdImageCreateFromGd2PartPtr(size, data, rect.x, rect.y, rect.width, rect.height)
                };
                return Image::of(at);
            }
            Format::Xbm => return Image::xbm(&mut copy),
        };
        // SAFETY: as above.
        Image::of(unsafe { read(size, data) })
    }

    /// `gdImageCreateFromXbm` over a `FILE *` the C library opens on the
    /// bytes, as php hands it the stream cast to one.
    fn xbm(bytes: &mut [u8]) -> Option<Image> {
        let mode = c"rb";
        if bytes.is_empty() {
            return None;
        }
        // SAFETY: a stream over this frame's bytes, closed below.
        let file =
            unsafe { libc_file::fmemopen(bytes.as_mut_ptr().cast(), bytes.len(), mode.as_ptr()) };
        if file.is_null() {
            return None;
        }
        // SAFETY: an open stream.
        let image = Image::of(unsafe { gdImageCreateFromXbm(file) });
        // SAFETY: the stream opened above, closed once.
        unsafe { libc_file::fclose(file) };
        image
    }
}

/// What a reader reads or an encoder writes: php's `PHP_GDIMG_TYPE_*`.
#[derive(Clone, Copy)]
pub(crate) enum Format {
    Png,
    Gif,
    Wbmp,
    /// With `gd.jpeg_ignore_warning`.
    Jpeg(bool),
    Webp,
    Avif,
    Tga,
    Bmp,
    Gd,
    Gd2,
    Gd2Part(Rect),
    Xbm,
}

/// How an encoder encodes, each with php's arguments.
#[derive(Clone, Copy)]
pub(crate) enum Encode {
    Gif,
    Png(i32),
    Jpeg(i32),
    Webp(i32),
    Avif(i32, i32),
    Wbmp(i32),
    Bmp(i32),
}

/// libgd's own formats, which php writes through a `FILE *` and not a
/// `gdIOCtx`, and which the keg's libgd refuses with a warning.
#[derive(Clone, Copy)]
pub(crate) enum GdFormat {
    Gd,
    Gd2(i32, i32),
}

/// A `gdImagePtr` named for the length of a call, the object holding it
/// alive throughout.
#[derive(Clone, Copy)]
pub(crate) struct Im(NonNull<GdImage>);

impl Im {
    fn at(self) -> Ptr {
        self.0.as_ptr()
    }

    fn fields(self) -> &'static GdImage {
        // SAFETY: the image lives while its object does, which outlives
        // the call this reference is read in.
        unsafe { self.0.as_ref() }
    }

    fn fields_mut(self) -> &'static mut GdImage {
        let mut at = self.0;
        // SAFETY: as `fields`; the call writes through no other alias.
        unsafe { at.as_mut() }
    }

    pub(crate) fn true_color(self) -> bool {
        self.fields().true_color != 0
    }

    pub(crate) fn sx(self) -> i32 {
        self.fields().sx
    }

    pub(crate) fn sy(self) -> i32 {
        self.fields().sy
    }

    /// `gdImageColorsTotal`.
    pub(crate) fn colors_total(self) -> i32 {
        self.fields().colors_total
    }

    /// `gdImageRed`, `gdImageGreen`, `gdImageBlue` and `gdImageAlpha`
    /// of `c`: a palette entry's, or a true color's channels.
    pub(crate) fn rgba(self, c: i32) -> [i32; 4] {
        let im = self.fields();
        if im.true_color != 0 {
            return [
                (c >> 16) & 0xFF,
                (c >> 8) & 0xFF,
                c & 0xFF,
                (c >> 24) & 0x7F,
            ];
        }
        let i = usize::try_from(c).unwrap_or(0).min(MAX_COLORS - 1);
        [im.red[i], im.green[i], im.blue[i], im.alpha[i]]
    }

    /// `im->red[c]` and its kin written, as `imagecolorset` writes them.
    pub(crate) fn set_palette(self, c: i32, [r, g, b, a]: [i32; 4]) {
        let im = self.fields_mut();
        let i = usize::try_from(c).unwrap_or(0).min(MAX_COLORS - 1);
        (im.red[i], im.green[i], im.blue[i], im.alpha[i]) = (r, g, b, a);
    }

    pub(crate) fn transparent(self) -> i32 {
        self.fields().transparent
    }

    pub(crate) fn interlaced(self) -> bool {
        self.fields().interlace != 0
    }

    pub(crate) fn aa(self) -> bool {
        self.fields().aa != 0
    }

    pub(crate) fn set_aa(self, on: bool) {
        self.fields_mut().aa = on.into();
    }

    pub(crate) fn interpolation_id(self) -> i32 {
        self.fields().interpolation_id
    }

    pub(crate) fn resolution(self) -> (u32, u32) {
        (self.fields().res_x, self.fields().res_y)
    }

    /// `imagecolorat`'s read: `tpixels[y][x]` or `pixels[y][x]` where the
    /// rows are there and the point is in bounds.
    pub(crate) fn pixel_at(self, x: i32, y: i32) -> Option<i32> {
        let im = self.fields();
        let rows_there = if im.true_color != 0 {
            !im.tpixels.is_null()
        } else {
            !im.pixels.is_null()
        };
        // SAFETY: a bounds check on the live image.
        if !rows_there || unsafe { gdImageBoundsSafe(self.at(), x, y) } == 0 {
            return None;
        }
        let (row, col) = (usize::try_from(y).ok()?, usize::try_from(x).ok()?);
        let (sx, sy) = (usize::try_from(im.sx).ok()?, usize::try_from(im.sy).ok()?);
        if im.true_color != 0 {
            // SAFETY: libgd allocates `sy` rows.
            let rows = unsafe { std::slice::from_raw_parts(im.tpixels, sy) };
            let line = *rows.get(row)?;
            // SAFETY: each row is `sx` pixels.
            let line = unsafe { std::slice::from_raw_parts(line, sx) };
            line.get(col).copied()
        } else {
            // SAFETY: as above.
            let rows = unsafe { std::slice::from_raw_parts(im.pixels, sy) };
            let line = *rows.get(row)?;
            // SAFETY: as above.
            let line = unsafe { std::slice::from_raw_parts(line, sx) };
            line.get(col).copied().map(i32::from)
        }
    }

    /// `gdImageGetPixel`.
    pub(crate) fn get_pixel(self, x: i32, y: i32) -> i32 {
        // SAFETY: libgd bounds-checks the point.
        unsafe { gdImageGetPixel(self.at(), x, y) }
    }

    /// `gdImageSetPixel`.
    pub(crate) fn set_pixel(self, x: i32, y: i32, c: i32) {
        // SAFETY: as `get_pixel`.
        unsafe { gdImageSetPixel(self.at(), x, y, c) };
    }

    /// `gdImageSetStyle`, which copies the style.
    pub(crate) fn set_style(self, style: &mut [i32]) {
        let n = c_int::try_from(style.len()).unwrap_or(c_int::MAX);
        // SAFETY: `n` ints at the pointer, copied by libgd.
        unsafe { gdImageSetStyle(self.at(), style.as_mut_ptr(), n) };
    }

    pub(crate) fn true_color_to_palette(self, dither: bool, colors: i32) -> bool {
        // SAFETY: a live image.
        unsafe { gdImageTrueColorToPalette(self.at(), dither.into(), colors) != 0 }
    }

    pub(crate) fn palette_to_true_color(self) -> bool {
        // SAFETY: a live image.
        unsafe { gdImagePaletteToTrueColor(self.at()) != 0 }
    }

    pub(crate) fn color_match(self, palette: Im) -> i32 {
        // SAFETY: two live images.
        unsafe { gdImageColorMatch(self.at(), palette.at()) }
    }

    pub(crate) fn set_thickness(self, thick: i32) {
        // SAFETY: a live image.
        unsafe { gdImageSetThickness(self.at(), thick) };
    }

    pub(crate) fn alpha_blending(self, on: i32) {
        // SAFETY: a live image.
        unsafe { gdImageAlphaBlending(self.at(), on) };
    }

    pub(crate) fn save_alpha(self, on: bool) {
        // SAFETY: a live image.
        unsafe { gdImageSaveAlpha(self.at(), on.into()) };
    }

    pub(crate) fn interlace(self, on: bool) {
        // SAFETY: a live image.
        unsafe { gdImageInterlace(self.at(), on.into()) };
    }

    pub(crate) fn set_transparent(self, c: i32) {
        // SAFETY: a live image; libgd checks the index.
        unsafe { gdImageColorTransparent(self.at(), c) };
    }

    pub(crate) fn set_anti_aliased(self, c: i32) {
        // SAFETY: a live image.
        unsafe { gdImageSetAntiAliased(self.at(), c) };
    }

    pub(crate) fn set_tile(self, tile: Im) {
        // SAFETY: two live images; libgd keeps the pointer, as php's does.
        unsafe { gdImageSetTile(self.at(), tile.at()) };
    }

    pub(crate) fn set_brush(self, brush: Im) {
        // SAFETY: as `set_tile`.
        unsafe { gdImageSetBrush(self.at(), brush.at()) };
    }

    pub(crate) fn palette_copy(self, src: Im) {
        // SAFETY: two live images.
        unsafe { gdImagePaletteCopy(self.at(), src.at()) };
    }

    pub(crate) fn color_deallocate(self, c: i32) {
        // SAFETY: a live image; the caller checked the index.
        unsafe { gdImageColorDeallocate(self.at(), c) };
    }

    /// The color calls: allocate, resolve, closest, exact, with alpha,
    /// and closest by hue, white and blackness.
    pub(crate) fn color(self, op: ColorOp, [r, g, b, a]: [i32; 4]) -> i32 {
        let at = self.at();
        match op {
            // SAFETY: a live image; libgd answers -1 where none fits.
            ColorOp::Allocate => unsafe { gdImageColorAllocate(at, r, g, b) },
            // SAFETY: as above.
            ColorOp::AllocateAlpha => unsafe { gdImageColorAllocateAlpha(at, r, g, b, a) },
            // SAFETY: as above.
            ColorOp::Resolve => unsafe { gdImageColorResolveAlpha(at, r, g, b, a) },
            // SAFETY: as above.
            ColorOp::Closest => unsafe { gdImageColorClosestAlpha(at, r, g, b, a) },
            // SAFETY: as above.
            ColorOp::Exact => unsafe { gdImageColorExactAlpha(at, r, g, b, a) },
            // SAFETY: as above.
            ColorOp::Hwb => unsafe { gdImageColorClosestHWB(at, r, g, b) },
        }
    }

    /// The two-point and box drawing calls.
    pub(crate) fn draw(self, op: Draw, [x1, y1, x2, y2]: [i32; 4], c: i32) {
        let at = self.at();
        match op {
            // SAFETY: a live image; libgd clips to it.
            Draw::Line => unsafe { gdImageLine(at, x1, y1, x2, y2, c) },
            // SAFETY: as above.
            Draw::Dashed => unsafe { gdImageDashedLine(at, x1, y1, x2, y2, c) },
            // SAFETY: as above.
            Draw::Rectangle => unsafe { gdImageRectangle(at, x1, y1, x2, y2, c) },
            // SAFETY: as above.
            Draw::Filled => unsafe { gdImageFilledRectangle(at, x1, y1, x2, y2, c) },
            // SAFETY: as above.
            Draw::Ellipse => unsafe { gdImageEllipse(at, x1, y1, x2, y2, c) },
            // SAFETY: as above.
            Draw::FilledEllipse => unsafe { gdImageFilledEllipse(at, x1, y1, x2, y2, c) },
            // SAFETY: as above.
            Draw::FillToBorder => unsafe { gdImageFillToBorder(at, x1, y1, x2, c) },
            // SAFETY: as above.
            Draw::Fill => unsafe { gdImageFill(at, x1, y1, c) },
            // SAFETY: as above.
            Draw::Clip => unsafe { gdImageSetClip(at, x1, y1, x2, y2) },
        }
    }

    /// `gdImageArc` and `gdImageFilledArc`, the latter where `style` is
    /// given.
    pub(crate) fn arc(self, [cx, cy, w, h, s, e]: [i32; 6], c: i32, style: Option<i32>) {
        match style {
            // SAFETY: a live image; libgd clips to it.
            Some(style) => unsafe { gdImageFilledArc(self.at(), cx, cy, w, h, s, e, c, style) },
            // SAFETY: as above.
            None => unsafe { gdImageArc(self.at(), cx, cy, w, h, s, e, c) },
        }
    }

    /// `gdImageOpenPolygon` (-1), `gdImagePolygon` (0) and
    /// `gdImageFilledPolygon` (1).
    pub(crate) fn polygon(self, points: &mut [Point], c: i32, filled: i32) {
        let n = c_int::try_from(points.len()).unwrap_or(c_int::MAX);
        let p = points.as_mut_ptr();
        match filled {
            // SAFETY: `n` points at `p`, read by libgd.
            -1 => unsafe { gdImageOpenPolygon(self.at(), p, n, c) },
            // SAFETY: as above.
            0 => unsafe { gdImagePolygon(self.at(), p, n, c) },
            // SAFETY: as above.
            _ => unsafe { gdImageFilledPolygon(self.at(), p, n, c) },
        }
    }

    /// `gdImageCopy` (no `dst_size`, no `pct`), `gdImageCopyMerge`,
    /// `gdImageCopyMergeGray`, `gdImageCopyResized` and
    /// `gdImageCopyResampled`.
    pub(crate) fn copy(
        self,
        src: Im,
        op: Copy,
        [dx, dy, sx, sy]: [i32; 4],
        [dw, dh, sw, sh]: [i32; 4],
    ) {
        let (d, s) = (self.at(), src.at());
        match op {
            // SAFETY: two live images; libgd clips to both.
            Copy::Plain => unsafe { gdImageCopy(d, s, dx, dy, sx, sy, sw, sh) },
            // SAFETY: as above; `dw` carries the percentage.
            Copy::Merge => unsafe { gdImageCopyMerge(d, s, dx, dy, sx, sy, sw, sh, dw) },
            // SAFETY: as above.
            Copy::MergeGray => unsafe { gdImageCopyMergeGray(d, s, dx, dy, sx, sy, sw, sh, dw) },
            // SAFETY: as above.
            Copy::Resized => unsafe { gdImageCopyResized(d, s, dx, dy, sx, sy, dw, dh, sw, sh) },
            // SAFETY: as above.
            Copy::Resampled => unsafe {
                gdImageCopyResampled(d, s, dx, dy, sx, sy, dw, dh, sw, sh);
            },
        }
    }

    /// `gdImageGetClip`.
    pub(crate) fn clip(self) -> [i32; 4] {
        let mut c = [0; 4];
        let [x1, y1, x2, y2] = c.each_mut().map(ptr::from_mut);
        // SAFETY: four slots of this frame.
        unsafe { gdImageGetClip(self.at(), x1, y1, x2, y2) };
        c
    }

    /// `gdImageChar` (`up` false) and `gdImageCharUp` over `font`.
    pub(crate) fn char(self, font: &Font, up: bool, [x, y]: [i32; 2], ch: i32, color: i32) {
        let mut f = font.raw();
        let fp = &raw mut f;
        if up {
            // SAFETY: a live image and a font whose data lives through the call.
            unsafe { gdImageCharUp(self.at(), fp, x, y, ch, color) };
        } else {
            // SAFETY: as above.
            unsafe { gdImageChar(self.at(), fp, x, y, ch, color) };
        }
    }

    /// The filters that take no argument, by `IMG_FILTER_*`.
    pub(crate) fn filter(self, which: i64, ints: [i32; 4], weight: f64) -> i32 {
        let at = self.at();
        let [a, b, c, d] = ints;
        #[allow(clippy::cast_possible_truncation)]
        let weight = weight as c_float;
        match which {
            // SAFETY: a live image.
            0 => unsafe { gdImageNegate(at) },
            // SAFETY: as above.
            1 => unsafe { gdImageGrayScale(at) },
            // SAFETY: as above.
            2 => unsafe { gdImageBrightness(at, a) },
            // SAFETY: as above.
            3 => unsafe { gdImageContrast(at, f64::from(a)) },
            // SAFETY: as above.
            4 => unsafe { gdImageColor(at, a, b, c, d) },
            // SAFETY: as above.
            5 => unsafe { gdImageEdgeDetectQuick(at) },
            // SAFETY: as above.
            6 => unsafe { gdImageEmboss(at) },
            // SAFETY: as above.
            7 => unsafe { gdImageGaussianBlur(at) },
            // SAFETY: as above.
            8 => unsafe { gdImageSelectiveBlur(at) },
            // SAFETY: as above.
            9 => unsafe { gdImageMeanRemoval(at) },
            // SAFETY: as above.
            10 => unsafe { gdImageSmooth(at, weight) },
            // SAFETY: as above.
            11 => unsafe { gdImagePixelate(at, a, c_uint::from(b != 0)) },
            // SAFETY: as above.
            _ => unsafe { gdImageScatter(at, a, b) },
        }
    }

    /// `gdImageScatterColor`.
    pub(crate) fn scatter_color(self, sub: i32, plus: i32, colors: &mut [i32]) -> bool {
        let n = c_uint::try_from(colors.len()).unwrap_or(c_uint::MAX);
        // SAFETY: `n` ints at the pointer, read by libgd.
        unsafe { gdImageScatterColor(self.at(), sub, plus, colors.as_mut_ptr(), n) != 0 }
    }

    /// `gdImageConvolution`.
    pub(crate) fn convolution(self, mut matrix: [[f32; 3]; 3], div: f32, offset: f32) -> bool {
        // SAFETY: a 3x3 matrix of this frame.
        unsafe { gdImageConvolution(self.at(), matrix.as_mut_ptr(), div, offset) != 0 }
    }

    /// `gdImageFlipVertical` (2), `gdImageFlipHorizontal` (1),
    /// `gdImageFlipBoth` (3).
    pub(crate) fn flip(self, mode: i64) {
        match mode {
            // SAFETY: a live image.
            1 => unsafe { gdImageFlipHorizontal(self.at()) },
            // SAFETY: as above.
            2 => unsafe { gdImageFlipVertical(self.at()) },
            // SAFETY: as above.
            _ => unsafe { gdImageFlipBoth(self.at()) },
        }
    }

    /// `gdImageRotateInterpolated`: for a palette image under
    /// `GD_NEAREST_NEIGHBOUR`, at an angle no square case takes, the
    /// rotation is made here from what libgd exports, and every other
    /// call is libgd's.
    pub(crate) fn rotate(self, angle: f32, bg: i32) -> Option<Image> {
        if !self.rotates_palette_by_nearest(angle, bg) {
            // SAFETY: a live image; libgd answers a new one or NULL.
            return Image::of(unsafe { gdImageRotateInterpolated(self.at(), angle, bg) });
        }
        let bg = match usize::try_from(bg) {
            Ok(i) if i < MAX_COLORS => {
                let im = self.fields();
                // `gdTrueColorAlpha`, as libgd composes it.
                (im.alpha[i] << 24)
                    .wrapping_add(im.red[i] << 16)
                    .wrapping_add(im.green[i] << 8)
                    .wrapping_add(im.blue[i])
            }
            _ => bg,
        };
        // SAFETY: a live image; libgd answers a new one or NULL.
        let clone = Image::of(unsafe { gdImageClone(self.at()) })?;
        if !clone.im().palette_to_true_color() {
            return None;
        }
        // SAFETY: the clone is a live truecolor image, which libgd
        // rotates without the palette path; it answers a new one or NULL.
        Image::of(unsafe { gdImageRotateInterpolated(clone.im().at(), angle, bg) })
    }

    /// Whether this image and angle are the ones `rotate` makes here, as
    /// `gdImageRotateInterpolated` reads them: its checks in its order,
    /// `angle_rounded` as it rounds it.
    fn rotates_palette_by_nearest(self, angle: f32, bg: i32) -> bool {
        /// `GD_NEAREST_NEIGHBOUR`.
        const NEAREST_NEIGHBOUR: c_int = 16;
        #[allow(clippy::cast_possible_truncation)]
        let hundredths = (angle * 100.0).floor() as i32;
        let square = matches!(
            hundredths % 36_000,
            0 | 9_000 | -9_000 | 18_000 | -18_000 | 27_000 | -27_000
        );
        !self.true_color() && bg >= 0 && !square && self.interpolation_id() == NEAREST_NEIGHBOUR
    }

    pub(crate) fn crop(self, rect: Rect) -> Option<Image> {
        // SAFETY: a live image and a rectangle of this frame.
        Image::of(unsafe { gdImageCrop(self.at(), &raw const rect) })
    }

    pub(crate) fn crop_auto(self, mode: u32) -> Option<Image> {
        // SAFETY: a live image.
        Image::of(unsafe { gdImageCropAuto(self.at(), mode) })
    }

    pub(crate) fn crop_threshold(self, color: u32, threshold: f32) -> Option<Image> {
        // SAFETY: a live image.
        Image::of(unsafe { gdImageCropThreshold(self.at(), color, threshold) })
    }

    pub(crate) fn set_interpolation(self, method: i32) -> bool {
        // SAFETY: a live image; libgd checks the method.
        unsafe { gdImageSetInterpolationMethod(self.at(), method) != 0 }
    }

    pub(crate) fn interpolation(self) -> i32 {
        // SAFETY: a live image.
        unsafe { gdImageGetInterpolationMethod(self.at()) }
    }

    pub(crate) fn scale(self, w: u32, h: u32) -> Option<Image> {
        // SAFETY: a live image.
        Image::of(unsafe { gdImageScale(self.at(), w, h) })
    }

    /// `gdTransformAffineGetImage`: the new image, `None` where libgd
    /// answered false or no image.
    pub(crate) fn affine(self, area: Option<Rect>, affine: &[f64; 6]) -> Option<Image> {
        let mut dst = ptr::null_mut();
        let mut rect = area.unwrap_or_default();
        let area = if area.is_some() {
            &raw mut rect
        } else {
            ptr::null_mut()
        };
        // SAFETY: a live image, a slot and a rectangle of this frame.
        let ok =
            unsafe { gdTransformAffineGetImage(&raw mut dst, self.at(), area, affine.as_ptr()) };
        let image = Image::of(dst);
        if ok != 1 {
            return None;
        }
        image
    }

    pub(crate) fn set_resolution(self, x: u32, y: u32) {
        // SAFETY: a live image.
        unsafe { gdImageSetResolution(self.at(), x, y) };
    }

    /// The image encoded through a `gdIOCtx` over `put`, as libgd writes
    /// it: what libgd reported after its last write.
    pub(crate) fn write(self, how: Encode, put: &mut Put<'_>) -> Reported {
        let at = self.at();
        writing(put, |ctx| match how {
            // SAFETY: a live image and the `gdIOCtx` of `writing`.
            Encode::Gif => unsafe { gdImageGifCtx(at, ctx) },
            // SAFETY: as above.
            Encode::Png(level) => unsafe { gdImagePngCtxEx(at, ctx, level) },
            // SAFETY: as above.
            Encode::Jpeg(q) => unsafe { gdImageJpegCtx(at, ctx, q) },
            // SAFETY: as above.
            Encode::Webp(q) => unsafe { gdImageWebpCtx(at, ctx, q) },
            // SAFETY: as above.
            Encode::Avif(q, speed) => unsafe { gdImageAvifCtx(at, ctx, q, speed) },
            // SAFETY: as above.
            Encode::Wbmp(fg) => unsafe { gdImageWBMPCtx(at, fg, ctx) },
            // SAFETY: as above.
            Encode::Bmp(c) => unsafe { gdImageBmpCtx(at, ctx, c) },
        })
    }

    /// `gdImageXbmCtx` through a `gdIOCtx` over `put`, the bitmap named
    /// after `name`: what libgd reported after its last write.
    pub(crate) fn write_xbm(self, name: &[u8], fg: i32, put: &mut Put<'_>) -> Reported {
        let mut name = c_string(name).into_bytes_with_nul();
        let at = self.at();
        writing(put, |ctx| {
            // SAFETY: a live image, a NUL-terminated name and the `gdIOCtx`.
            unsafe { gdImageXbmCtx(at, name.as_mut_ptr().cast(), fg, ctx) };
        })
    }

    /// The image in one of libgd's own formats, the bytes its `Ptr`
    /// encoder answered; `None` where it answered none.
    pub(crate) fn encode(self, how: GdFormat) -> Option<Vec<u8>> {
        let mut size: c_int = 0;
        let s = &raw mut size;
        let at = self.at();
        let data = match how {
            // SAFETY: a live image and a slot of this frame.
            GdFormat::Gd => unsafe { gdImageGdPtr(at, s) },
            // SAFETY: as above.
            GdFormat::Gd2(cs, fmt) => unsafe { gdImageGd2Ptr(at, cs, fmt, s) },
        };
        taken(data, size)
    }

    /// `gdImageStringFT`, or `gdImageStringFTEx` where `linespacing` is
    /// given, on this image or, for `None`, measuring alone: the bounding
    /// box, or libgd's error text.
    pub(crate) fn string_ft(im: Option<Im>, text: &Ft) -> Result<[i32; 8], Vec<u8>> {
        let mut brect = [0 as c_int; 8];
        let at = im.map_or(ptr::null_mut(), Im::at);
        let (font, string) = (c_string(&text.font), c_string(&text.text));
        let error = match text.linespacing {
            Some(spacing) => {
                let mut extra = FtExtra {
                    flags: i32::from(spacing.is_some()),
                    linespacing: spacing.unwrap_or(0.0),
                    charmap: 0,
                    hdpi: 0,
                    vdpi: 0,
                    xshow: ptr::null_mut(),
                    fontpath: ptr::null_mut(),
                };
                // SAFETY: a live image or NULL, slots and strings of this frame.
                unsafe {
                    gdImageStringFTEx(
                        at,
                        brect.as_mut_ptr(),
                        text.color,
                        font.as_ptr(),
                        text.size,
                        text.angle,
                        text.x,
                        text.y,
                        string.as_ptr(),
                        &raw mut extra,
                    )
                }
            }
            // SAFETY: as above.
            None => unsafe {
                gdImageStringFT(
                    at,
                    brect.as_mut_ptr(),
                    text.color,
                    font.as_ptr(),
                    text.size,
                    text.angle,
                    text.x,
                    text.y,
                    string.as_ptr(),
                )
            },
        };
        if error.is_null() {
            Ok(brect)
        } else {
            Err(self::text(error))
        }
    }
}

/// What `php_imagettftext_common` hands libgd: `linespacing` is the
/// options array's, `Some(None)` for an array without the key.
pub(crate) struct Ft {
    pub(crate) size: f64,
    pub(crate) angle: f64,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) color: i32,
    pub(crate) font: Vec<u8>,
    pub(crate) text: Vec<u8>,
    #[allow(clippy::option_option)]
    pub(crate) linespacing: Option<Option<f64>>,
}

/// The color calls of [`Im::color`].
#[derive(Clone, Copy)]
pub(crate) enum ColorOp {
    Allocate,
    AllocateAlpha,
    Resolve,
    Closest,
    Exact,
    Hwb,
}

/// The drawing calls of [`Im::draw`].
#[derive(Clone, Copy)]
pub(crate) enum Draw {
    Line,
    Dashed,
    Rectangle,
    Filled,
    Ellipse,
    FilledEllipse,
    FillToBorder,
    Fill,
    Clip,
}

/// The copies of [`Im::copy`].
#[derive(Clone, Copy)]
pub(crate) enum Copy {
    Plain,
    Merge,
    MergeGray,
    Resized,
    Resampled,
}

/// The affine matrices `imageaffinematrixget` asks for, and their
/// concatenation: `None` where libgd answered false.
pub(crate) fn affine_matrix(kind: i64, x: f64, y: f64) -> Option<[f64; 6]> {
    let mut m = [0.0; 6];
    let p = m.as_mut_ptr();
    let ok = match kind {
        // SAFETY: six doubles of this frame.
        0 => unsafe { gdAffineTranslate(p, x, y) },
        // SAFETY: as above.
        1 => unsafe { gdAffineScale(p, x, y) },
        // SAFETY: as above.
        2 => unsafe { gdAffineRotate(p, x) },
        // SAFETY: as above.
        3 => unsafe { gdAffineShearHorizontal(p, x) },
        // SAFETY: as above.
        _ => unsafe { gdAffineShearVertical(p, x) },
    };
    (ok != 0).then_some(m)
}

/// `gdAffineConcat`.
pub(crate) fn affine_concat(m1: &[f64; 6], m2: &[f64; 6]) -> Option<[f64; 6]> {
    let mut m = [0.0; 6];
    // SAFETY: three arrays of six doubles of this frame.
    let ok = unsafe { gdAffineConcat(m.as_mut_ptr(), m1.as_ptr(), m2.as_ptr()) };
    (ok == 1).then_some(m)
}

// ------------------------------------------------------------ fonts

/// A font: one of libgd's five, or one `imageloadfont` read, whose
/// glyphs the [`Glyphs`] hold.
pub(crate) enum Font<'a> {
    Builtin(i64),
    Loaded(&'a Glyphs),
}

/// A font `imageloadfont` read: the header's numbers and the glyph
/// bytes, one per pixel.
pub(crate) struct Glyphs {
    pub(crate) nchars: i32,
    pub(crate) offset: i32,
    pub(crate) w: i32,
    pub(crate) h: i32,
    /// `emalloc(sizeof(gdFont))`, held so the heap counts it as php's.
    pub(crate) header: php_ext::values::Emalloc,
    pub(crate) data: php_ext::values::Emalloc,
}

impl Font<'_> {
    fn builtin(n: i64) -> *mut GdFont {
        match n {
            // SAFETY: libgd's static fonts.
            ..=1 => unsafe { gdFontGetTiny() },
            // SAFETY: as above.
            2 => unsafe { gdFontGetSmall() },
            // SAFETY: as above.
            3 => unsafe { gdFontGetMediumBold() },
            // SAFETY: as above.
            4 => unsafe { gdFontGetLarge() },
            // SAFETY: as above.
            _ => unsafe { gdFontGetGiant() },
        }
    }

    /// The `gdFont` handed libgd for one call.
    fn raw(&self) -> GdFont {
        match self {
            Font::Builtin(n) => {
                // SAFETY: libgd's static font, copied.
                unsafe { *Font::builtin(*n) }
            }
            Font::Loaded(g) => GdFont {
                nchars: g.nchars,
                offset: g.offset,
                w: g.w,
                h: g.h,
                data: g.data.bytes().as_ptr().cast_mut().cast(),
            },
        }
    }

    /// `font->w` and `font->h`.
    pub(crate) fn size(&self) -> (i32, i32) {
        let f = self.raw();
        (f.w, f.h)
    }

    /// `font->nchars`, `font->offset` and the glyph bytes, which php's
    /// `php_gdimagecharup` reads itself.
    pub(crate) fn glyphs(&self) -> (i32, i32, &[u8]) {
        let f = self.raw();
        let len = usize::try_from(f.nchars.saturating_mul(f.w).saturating_mul(f.h)).unwrap_or(0);
        match self {
            Font::Loaded(g) => (f.nchars, f.offset, g.data.bytes()),
            // SAFETY: libgd's static glyphs, `nchars * w * h` bytes.
            Font::Builtin(_) => (f.nchars, f.offset, unsafe {
                std::slice::from_raw_parts(f.data.cast::<u8>(), len)
            }),
        }
    }
}

/// The long php's `FLIPWORD` and `fread` read as a C `int` in the
/// machine's order.
pub(crate) fn native_int(bytes: [u8; 4]) -> i32 {
    i32::from_ne_bytes(bytes)
}
