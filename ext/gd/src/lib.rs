//! The `gd` extension: `php-src/ext/gd` as one crate under `ext/`
//! (ADR-0004), over the libgd the pinned keg links, 2.3.3, found
//! through `pkg-config gdlib` and bound in [`ffi`] alone, never vendored
//! (design §7.2, revision 75; #1573). The phpt subset is
//! `php-src/ext/gd/tests`, through `xtask phpt --subset`.
//!
//! The oracle builds gd against that external libgd, so this is `gd.c`
//! as it compiles without `HAVE_GD_BUNDLED`: `gd_info()`, `--ri gd` and
//! the `GD_*` constants say what the external library has. An image's
//! bytes, its pixels and `FreeType`'s rendering are libgd's; what is
//! ported is php's argument handling, the `GdImage` and `GdFont`
//! objects ([`image`]) and the stream plumbing of the readers and
//! writers ([`io`]). The drawing natives are [`draw`], the filters,
//! transforms and text [`transform`].
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

mod draw;
mod ffi;
mod image;
mod io;
mod throwables;
mod transform;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::attr::{php_const, php_function};
use php_ext::decl::{Bytes, IniDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::{Ini, Request};
use php_ext::values::{Owner, PhpArray, PhpString, Value};

/// `gd.jpeg_ignore_warning`, `PHP_INI_ALL`, on by default.
const JPEG_IGNORE_WARNING: &str = "gd.jpeg_ignore_warning";

/// `register_gd_symbols`: the constants in the order `gd.stub.php`
/// registers them under the oracle's build, the `@cvalue`s of `gd.h` and
/// `php_gd.h`, `GD_BUNDLED` zero and the `GD_*` version the keg's.
///
/// ```php
/// $c = get_defined_constants(true)['gd'];
/// echo count($c), "\n";
/// foreach (['IMG_AVIF', 'IMG_WEBP_LOSSLESS', 'IMG_COLOR_TRANSPARENT', 'GD_BUNDLED', 'GD_VERSION', 'GD_EXTRA_VERSION', 'IMG_TRIANGLE', 'PNG_ALL_FILTERS'] as $n) var_dump($c[$n]);
/// echo implode(',', array_slice(array_keys($c), 0, 12)), "\n";
/// ```
/// ```expect
/// 89
/// int(256)
/// int(101)
/// int(-6)
/// int(0)
/// string(5) "2.3.3"
/// string(0) ""
/// int(20)
/// int(248)
/// IMG_AVIF,IMG_GIF,IMG_JPG,IMG_JPEG,IMG_PNG,IMG_WBMP,IMG_XPM,IMG_WEBP,IMG_BMP,IMG_TGA,IMG_WEBP_LOSSLESS,IMG_COLOR_TILED
/// ```
#[php_const]
pub(crate) mod constants {
    pub(crate) const IMG_AVIF: i64 = 256;
    pub(crate) const IMG_GIF: i64 = 1;
    pub(crate) const IMG_JPG: i64 = 2;
    pub(crate) const IMG_JPEG: i64 = 2;
    pub(crate) const IMG_PNG: i64 = 4;
    pub(crate) const IMG_WBMP: i64 = 8;
    pub(crate) const IMG_XPM: i64 = 16;
    pub(crate) const IMG_WEBP: i64 = 32;
    pub(crate) const IMG_BMP: i64 = 64;
    pub(crate) const IMG_TGA: i64 = 128;
    pub(crate) const IMG_WEBP_LOSSLESS: i64 = 101;
    pub(crate) const IMG_COLOR_TILED: i64 = -5;
    pub(crate) const IMG_COLOR_STYLED: i64 = -2;
    pub(crate) const IMG_COLOR_BRUSHED: i64 = -3;
    pub(crate) const IMG_COLOR_STYLEDBRUSHED: i64 = -4;
    pub(crate) const IMG_COLOR_TRANSPARENT: i64 = -6;
    pub(crate) const IMG_ARC_ROUNDED: i64 = 0;
    pub(crate) const IMG_ARC_PIE: i64 = 0;
    pub(crate) const IMG_ARC_CHORD: i64 = 1;
    pub(crate) const IMG_ARC_NOFILL: i64 = 2;
    pub(crate) const IMG_ARC_EDGED: i64 = 4;
    pub(crate) const IMG_GD2_RAW: i64 = 1;
    pub(crate) const IMG_GD2_COMPRESSED: i64 = 2;
    pub(crate) const IMG_FLIP_HORIZONTAL: i64 = 1;
    pub(crate) const IMG_FLIP_VERTICAL: i64 = 2;
    pub(crate) const IMG_FLIP_BOTH: i64 = 3;
    pub(crate) const IMG_EFFECT_REPLACE: i64 = 0;
    pub(crate) const IMG_EFFECT_ALPHABLEND: i64 = 1;
    pub(crate) const IMG_EFFECT_NORMAL: i64 = 2;
    pub(crate) const IMG_EFFECT_OVERLAY: i64 = 3;
    pub(crate) const IMG_EFFECT_MULTIPLY: i64 = 4;
    pub(crate) const IMG_CROP_DEFAULT: i64 = 0;
    pub(crate) const IMG_CROP_TRANSPARENT: i64 = 1;
    pub(crate) const IMG_CROP_BLACK: i64 = 2;
    pub(crate) const IMG_CROP_WHITE: i64 = 3;
    pub(crate) const IMG_CROP_SIDES: i64 = 4;
    pub(crate) const IMG_CROP_THRESHOLD: i64 = 5;
    pub(crate) const IMG_BELL: i64 = 1;
    pub(crate) const IMG_BESSEL: i64 = 2;
    pub(crate) const IMG_BILINEAR_FIXED: i64 = 3;
    pub(crate) const IMG_BICUBIC: i64 = 4;
    pub(crate) const IMG_BICUBIC_FIXED: i64 = 5;
    pub(crate) const IMG_BLACKMAN: i64 = 6;
    pub(crate) const IMG_BOX: i64 = 7;
    pub(crate) const IMG_BSPLINE: i64 = 8;
    pub(crate) const IMG_CATMULLROM: i64 = 9;
    pub(crate) const IMG_GAUSSIAN: i64 = 10;
    pub(crate) const IMG_GENERALIZED_CUBIC: i64 = 11;
    pub(crate) const IMG_HERMITE: i64 = 12;
    pub(crate) const IMG_HAMMING: i64 = 13;
    pub(crate) const IMG_HANNING: i64 = 14;
    pub(crate) const IMG_MITCHELL: i64 = 15;
    pub(crate) const IMG_POWER: i64 = 17;
    pub(crate) const IMG_QUADRATIC: i64 = 18;
    pub(crate) const IMG_SINC: i64 = 19;
    pub(crate) const IMG_NEAREST_NEIGHBOUR: i64 = 16;
    pub(crate) const IMG_WEIGHTED4: i64 = 21;
    pub(crate) const IMG_TRIANGLE: i64 = 20;
    pub(crate) const IMG_AFFINE_TRANSLATE: i64 = 0;
    pub(crate) const IMG_AFFINE_SCALE: i64 = 1;
    pub(crate) const IMG_AFFINE_ROTATE: i64 = 2;
    pub(crate) const IMG_AFFINE_SHEAR_HORIZONTAL: i64 = 3;
    pub(crate) const IMG_AFFINE_SHEAR_VERTICAL: i64 = 4;
    pub(crate) const GD_BUNDLED: i64 = 0;
    pub(crate) const IMG_FILTER_NEGATE: i64 = 0;
    pub(crate) const IMG_FILTER_GRAYSCALE: i64 = 1;
    pub(crate) const IMG_FILTER_BRIGHTNESS: i64 = 2;
    pub(crate) const IMG_FILTER_CONTRAST: i64 = 3;
    pub(crate) const IMG_FILTER_COLORIZE: i64 = 4;
    pub(crate) const IMG_FILTER_EDGEDETECT: i64 = 5;
    pub(crate) const IMG_FILTER_GAUSSIAN_BLUR: i64 = 7;
    pub(crate) const IMG_FILTER_SELECTIVE_BLUR: i64 = 8;
    pub(crate) const IMG_FILTER_EMBOSS: i64 = 6;
    pub(crate) const IMG_FILTER_MEAN_REMOVAL: i64 = 9;
    pub(crate) const IMG_FILTER_SMOOTH: i64 = 10;
    pub(crate) const IMG_FILTER_PIXELATE: i64 = 11;
    pub(crate) const IMG_FILTER_SCATTER: i64 = 12;
    pub(crate) const GD_VERSION: &str = "2.3.3";
    pub(crate) const GD_MAJOR_VERSION: i64 = 2;
    pub(crate) const GD_MINOR_VERSION: i64 = 3;
    pub(crate) const GD_RELEASE_VERSION: i64 = 3;
    pub(crate) const GD_EXTRA_VERSION: &str = "";
    pub(crate) const PNG_NO_FILTER: i64 = 0x00;
    pub(crate) const PNG_FILTER_NONE: i64 = 0x08;
    pub(crate) const PNG_FILTER_SUB: i64 = 0x10;
    pub(crate) const PNG_FILTER_UP: i64 = 0x20;
    pub(crate) const PNG_FILTER_AVG: i64 = 0x40;
    pub(crate) const PNG_FILTER_PAETH: i64 = 0x80;
    pub(crate) const PNG_ALL_FILTERS: i64 = 0x08 | 0x10 | 0x20 | 0x40 | 0x80;
}

/// The extension value (design §7.1): the ini entry, the constants, the
/// natives in `gd.stub.php`'s order, the two classes, and the font
/// cache `RSHUTDOWN` empties. No allocator is set on libgd, so the
/// objects alone are on the request's heap.
///
/// ```php
/// var_dump(extension_loaded('gd'), ini_get('gd.jpeg_ignore_warning'));
/// $m = memory_get_usage();
/// $i = imagecreatetruecolor(500, 500);
/// var_dump(memory_get_usage() - $m);
/// ```
/// ```expect
/// bool(true)
/// string(1) "1"
/// int(48)
/// ```
#[allow(clippy::too_many_lines)] // the stub's natives, one line each
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .ini(vec![IniDecl {
            name: Name(Bytes::borrowed(JPEG_IGNORE_WARNING.as_bytes())),
            default: Some(Bytes::borrowed(b"1")),
            user: true,
            per_dir: true,
            system: true,
        }])
        .constants(constants::DECLARED)
        .functions(&[
            gd_info::DECLARED,
            image::imageloadfont::DECLARED,
            draw::imagesetstyle::DECLARED,
            image::imagecreatetruecolor::DECLARED,
            image::imageistruecolor::DECLARED,
            draw::imagetruecolortopalette::DECLARED,
            draw::imagepalettetotruecolor::DECLARED,
            draw::imagecolormatch::DECLARED,
            draw::imagesetthickness::DECLARED,
            draw::imagefilledellipse::DECLARED,
            draw::imagefilledarc::DECLARED,
            draw::imagealphablending::DECLARED,
            draw::imagesavealpha::DECLARED,
            draw::imagelayereffect::DECLARED,
            draw::imagecolorallocatealpha::DECLARED,
            draw::imagecolorresolvealpha::DECLARED,
            draw::imagecolorclosestalpha::DECLARED,
            draw::imagecolorexactalpha::DECLARED,
            draw::imagecopyresampled::DECLARED,
            transform::imagerotate::DECLARED,
            draw::imagesettile::DECLARED,
            draw::imagesetbrush::DECLARED,
            image::imagecreate::DECLARED,
            io::imagetypes::DECLARED,
            io::imagecreatefromstring::DECLARED,
            io::imagecreatefromavif::DECLARED,
            io::imagecreatefromgif::DECLARED,
            io::imagecreatefromjpeg::DECLARED,
            io::imagecreatefrompng::DECLARED,
            io::imagecreatefromwebp::DECLARED,
            io::imagecreatefromxbm::DECLARED,
            io::imagecreatefromwbmp::DECLARED,
            io::imagecreatefromgd::DECLARED,
            io::imagecreatefromgd2::DECLARED,
            io::imagecreatefromgd2part::DECLARED,
            io::imagecreatefrombmp::DECLARED,
            io::imagecreatefromtga::DECLARED,
            io::imagexbm::DECLARED,
            io::imageavif::DECLARED,
            io::imagegif::DECLARED,
            io::imagepng::DECLARED,
            io::imagewebp::DECLARED,
            io::imagejpeg::DECLARED,
            io::imagewbmp::DECLARED,
            io::imagegd::DECLARED,
            io::imagegd2::DECLARED,
            io::imagebmp::DECLARED,
            io::imagedestroy::DECLARED,
            draw::imagecolorallocate::DECLARED,
            draw::imagepalettecopy::DECLARED,
            draw::imagecolorat::DECLARED,
            draw::imagecolorclosest::DECLARED,
            draw::imagecolorclosesthwb::DECLARED,
            draw::imagecolordeallocate::DECLARED,
            draw::imagecolorresolve::DECLARED,
            draw::imagecolorexact::DECLARED,
            draw::imagecolorset::DECLARED,
            draw::imagecolorsforindex::DECLARED,
            draw::imagegammacorrect::DECLARED,
            draw::imagesetpixel::DECLARED,
            draw::imageline::DECLARED,
            draw::imagedashedline::DECLARED,
            draw::imagerectangle::DECLARED,
            draw::imagefilledrectangle::DECLARED,
            draw::imagearc::DECLARED,
            draw::imageellipse::DECLARED,
            draw::imagefilltoborder::DECLARED,
            draw::imagefill::DECLARED,
            draw::imagecolorstotal::DECLARED,
            draw::imagecolortransparent::DECLARED,
            draw::imageinterlace::DECLARED,
            draw::imagepolygon::DECLARED,
            draw::imageopenpolygon::DECLARED,
            draw::imagefilledpolygon::DECLARED,
            draw::imagefontwidth::DECLARED,
            draw::imagefontheight::DECLARED,
            draw::imagechar::DECLARED,
            draw::imagecharup::DECLARED,
            draw::imagestring::DECLARED,
            draw::imagestringup::DECLARED,
            draw::imagecopy::DECLARED,
            draw::imagecopymerge::DECLARED,
            draw::imagecopymergegray::DECLARED,
            draw::imagecopyresized::DECLARED,
            draw::imagesx::DECLARED,
            draw::imagesy::DECLARED,
            draw::imagesetclip::DECLARED,
            draw::imagegetclip::DECLARED,
            transform::imageftbbox::DECLARED,
            transform::imagefttext::DECLARED,
            transform::imagettfbbox::DECLARED,
            transform::imagettftext::DECLARED,
            transform::imagefilter::DECLARED,
            transform::imageconvolution::DECLARED,
            transform::imageflip::DECLARED,
            transform::imageantialias::DECLARED,
            transform::imagecrop::DECLARED,
            transform::imagecropauto::DECLARED,
            transform::imagescale::DECLARED,
            transform::imageaffine::DECLARED,
            transform::imageaffinematrixget::DECLARED,
            transform::imageaffinematrixconcat::DECLARED,
            transform::imagegetinterpolation::DECLARED,
            transform::imagesetinterpolation::DECLARED,
            transform::imageresolution::DECLARED,
        ])
        .class::<image::GdImage>()
        .class::<image::GdFont>()
        .engine_state(throwables::Throwables::start)
        .rshutdown(rshutdown)
        .info(info)
        .build("gd")
}

/// `PHP_RSHUTDOWN_FUNCTION(gd)`: libgd's `FreeType` cache emptied.
fn rshutdown(_req: &mut Request) {
    ffi::font_cache_shutdown();
}

/// What the oracle's build supports, in `gd_info()`'s order after
/// `FreeType Linkage`: no XPM and no JIS-mapped fonts, as the keg's
/// libgd is built.
const SUPPORT: [(&str, bool); 12] = [
    ("GIF Read Support", true),
    ("GIF Create Support", true),
    ("JPEG Support", true),
    ("PNG Support", true),
    ("WBMP Support", true),
    ("XPM Support", false),
    ("XBM Support", true),
    ("WebP Support", true),
    ("BMP Support", true),
    ("AVIF Support", true),
    ("TGA Read Support", true),
    ("JIS-mapped Japanese Font Support", false),
];

/// `PHP_MINFO_FUNCTION(gd)` without `HAVE_GD_BUNDLED`: the headers'
/// version and the library's, what the build supports, then
/// `DISPLAY_INI_ENTRIES` with the boolean displayer.
///
/// ```php
/// $e = new ReflectionExtension('gd'); ob_start(); $e->info(); echo ob_get_clean();
/// ```
/// ```expect
/// gd
///
/// GD Support => enabled
/// GD headers Version => 2.3.3
/// GD library Version => 2.3.3
/// FreeType Support => enabled
/// FreeType Linkage => with freetype
/// GIF Read Support => enabled
/// GIF Create Support => enabled
/// JPEG Support => enabled
/// PNG Support => enabled
/// WBMP Support => enabled
/// XBM Support => enabled
/// WebP Support => enabled
/// BMP Support => enabled
/// AVIF Support => enabled
/// TGA Read Support => enabled
///
/// Directive => Local Value => Master Value
/// gd.jpeg_ignore_warning => On => On
/// ```
fn info(req: &mut Request) {
    let library = ffi::version();
    let mut rows: Vec<[&str; 2]> = vec![
        ["GD Support", "enabled"],
        ["GD headers Version", constants::GD_VERSION],
        ["GD library Version", &library],
        ["FreeType Support", "enabled"],
        ["FreeType Linkage", "with freetype"],
    ];
    rows.extend(
        SUPPORT
            .iter()
            .filter(|(_, on)| *on)
            .map(|(n, _)| [*n, "enabled"]),
    );
    let cells: Vec<&[&str]> = rows.iter().map(<[&str; 2]>::as_slice).collect();
    req.extensions_mut().info_table(&cells);
    let entry = req
        .ini()
        .entries()
        .into_iter()
        .find(|e| e.decl.name.0.as_ref() == JPEG_IGNORE_WARNING.as_bytes());
    let shown = |v: Option<&[u8]>| {
        if v.is_some_and(Ini::parse_bool) {
            "On"
        } else {
            "Off"
        }
    };
    let local = shown(entry.as_ref().and_then(|e| e.value.as_deref()));
    let master = shown(entry.as_ref().and_then(|e| e.master.as_deref()));
    req.ini_mut()
        .info_ini_table(&[[JPEG_IGNORE_WARNING, local, master]]);
}

/// `gd_info(): array`: the version the headers name and what the build
/// supports.
///
/// ```php
/// var_dump(gd_info());
/// ```
/// ```expect
/// array(15) {
///   ["GD Version"]=>
///   string(5) "2.3.3"
///   ["FreeType Support"]=>
///   bool(true)
///   ["FreeType Linkage"]=>
///   string(13) "with freetype"
///   ["GIF Read Support"]=>
///   bool(true)
///   ["GIF Create Support"]=>
///   bool(true)
///   ["JPEG Support"]=>
///   bool(true)
///   ["PNG Support"]=>
///   bool(true)
///   ["WBMP Support"]=>
///   bool(true)
///   ["XPM Support"]=>
///   bool(false)
///   ["XBM Support"]=>
///   bool(true)
///   ["WebP Support"]=>
///   bool(true)
///   ["BMP Support"]=>
///   bool(true)
///   ["AVIF Support"]=>
///   bool(true)
///   ["TGA Read Support"]=>
///   bool(true)
///   ["JIS-mapped Japanese Font Support"]=>
///   bool(false)
/// }
/// ```
#[php_function]
fn gd_info(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let mut info = PhpArray::new(req)?;
    let version = PhpString::new(req, constants::GD_VERSION.as_bytes())?;
    set(req, &mut info, "GD Version", version.into_value())?;
    set(req, &mut info, "FreeType Support", Owner::bool(true))?;
    let linkage = PhpString::new(req, b"with freetype")?;
    set(req, &mut info, "FreeType Linkage", linkage.into_value())?;
    for (name, on) in SUPPORT {
        set(req, &mut info, name, Owner::bool(on))?;
    }
    Ok(info)
}

/// `add_assoc_*`: `value` under the string key `name`.
pub(crate) fn set(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    name: &str,
    value: Owner<Value>,
) -> Result<(), Thrown> {
    array.set_str(req, name.as_bytes(), value)
}

/// A packed array of `values`, `add_index_*` from zero.
pub(crate) fn list(req: &mut Request, values: &[Value]) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = PhpArray::new(req)?;
    for value in values {
        array.append(req, Owner::take(req, *value))?;
    }
    Ok(array)
}
