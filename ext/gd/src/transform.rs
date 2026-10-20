//! The natives that make a new image of one, or change one whole:
//! rotation, the filters, the convolution, flips, crops, scaling and the
//! affine transforms, the interpolation and resolution settings, and
//! `FreeType` text.

use std::ops::ControlFlow;

use php_ext::abi::{Instance, OrFalse, Raw, True, Union, Untyped, Variadic};
use php_ext::attr::php_function;
use php_ext::coerce::{coerce_param, to_double, to_long};
use php_ext::decl::Type;
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, argument_error, arity, docref, throw_error,
};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value, ValueRef};

use crate::ffi::{Ft, Im, Rect, affine_concat, affine_matrix};
use crate::image::{GdImage, gd, im, int, wrap};

/// `imagerotate(GdImage $image, float $angle, int $background_color):
/// GdImage|false`: libgd's interpolated rotation.
///
/// ```php
/// $i = imagecreatetruecolor(4, 2);
/// $r = imagerotate($i, 90, 0);
/// var_dump(imagesx($r), imagesy($r));
/// try { imagerotate($i, 1e9, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2)
/// int(4)
/// imagerotate(): Argument #2 ($angle) must be between -21474836 and 21474836
/// ```
#[php_function]
fn imagerotate(
    req: &mut Request,
    image: Instance<GdImage>,
    angle: f64,
    background_color: i64,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    let (min, max) = (i32::MIN / 100, i32::MAX / 100);
    if angle < f64::from(min) || angle > f64::from(max) {
        let text = format!("be between {min} and {max}");
        return Err(arg_value_error(req, 2, &text));
    }
    let im = im(req, image)?;
    #[allow(clippy::cast_possible_truncation)]
    let angle = angle as f32;
    let rotated = gd(req, || im.rotate(angle, int(background_color)))?;
    wrap(req, rotated)
}

/// `php_imagettftext_common`: the size held to `FreeType`'s 26.6 range,
/// the angle in radians, the options array's `linespacing`, then
/// libgd's text over the image or, for `None`, its box alone.
#[allow(clippy::too_many_arguments)]
fn ttf(
    req: &mut Request,
    im: Option<Im>,
    size: f64,
    angle: f64,
    at: [i64; 3],
    font: PhpString,
    text: PhpString,
    options: Option<PhpArray>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let (min, max) = ((i64::MIN as f64) / 64.0, (i64::MAX as f64) / 64.0);
    if size < min || size > max {
        #[allow(clippy::cast_possible_truncation)]
        let text = format!("be between {} and {}", min as i64, max as i64);
        return Err(arg_value_error(req, 2, &text));
    }
    if !size.is_finite() {
        return Err(arg_value_error(req, 2, "be finite"));
    }
    let linespacing = match options {
        None => None,
        Some(options) => {
            let mut spacing = None;
            if !options.is_packed() {
                // `ZEND_HASH_MAP_FOREACH_STR_KEY_VAL(Z_ARRVAL_P(EXT), key, item)`
                // (`Z/ext/gd/gd.c:3373`), no owner of the table taken: the
                // argument holds it while `zval_get_double` reaches a
                // diagnostic's handler.
                walk::foreach(req, options, Hold::None, |req, row| -> walk::Step {
                    let key = row.key().clone();
                    let item = row.value().deref();
                    if let Key::Str(key) = key
                        && key.as_bytes() == b"linespacing"
                    {
                        spacing = Some(to_double(req, &item)?);
                    }
                    Ok(ControlFlow::Continue(()))
                })?;
            }
            Some(spacing)
        }
    };
    let [x, y, color] = at.map(int);
    let ft = Ft {
        size,
        angle: angle * (std::f64::consts::PI / 180.0),
        x,
        y,
        color,
        font: font.as_bytes().to_vec(),
        text: text.as_bytes().to_vec(),
        linespacing,
    };
    match gd(req, || Im::string_ft(im, &ft))? {
        Ok(brect) => {
            let values = brect.map(|n| Value::int(i64::from(n)));
            Ok(OrFalse::Value(crate::list(req, &values)?))
        }
        Err(error) => {
            docref(req, Level::Warning, &error);
            Ok(OrFalse::False)
        }
    }
}

/// `imageftbbox(float $size, float $angle, string $font_filename, string
/// $string, array $options = []): array|false`, and its alias
/// `imagettfbbox`: the eight corners of the text's box.
///
/// ```php
/// var_dump(imageftbbox(12, 0, '/nonexistent.ttf', 'x'));
/// try { imageftbbox(NAN, 0, 'f', 'x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imagettfbbox(1e300, 0, 'f', 'x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: imageftbbox(): Could not find/open font in %s on line 2
/// bool(false)
/// imageftbbox(): Argument #2 ($angle) must be finite
/// imagettfbbox(): Argument #2 ($angle) must be between -144115188075855872 and 144115188075855872
/// ```
#[php_function(aliases(imagettfbbox))]
fn imageftbbox(
    req: &mut Request,
    size: f64,
    angle: f64,
    font_filename: PhpString,
    string: PhpString,
    #[php(default = [])] options: PhpArray,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let options = (req.frames().argument_count() > 4).then_some(options);
    ttf(
        req,
        None,
        size,
        angle,
        [0, 0, -1],
        font_filename,
        string,
        options,
    )
}

/// `imagefttext(GdImage $image, float $size, float $angle, int $x, int
/// $y, int $color, string $font_filename, string $text, array $options =
/// []): array|false`, and its alias `imagettftext`.
///
/// ```php
/// $i = imagecreatetruecolor(10, 10);
/// var_dump(imagefttext($i, 12, 0, 0, 0, 0xFF, '/nonexistent.ttf', 'x', ['linespacing' => 2]));
/// var_dump(imagettftext($i, 12, 0, 0, 0, 0xFF, '/nonexistent.ttf', 'x'));
/// ```
/// ```expect
/// Warning: imagefttext(): Could not find/open font in %s on line 3
/// bool(false)
///
/// Warning: imagettftext(): Could not find/open font in %s on line 4
/// bool(false)
/// ```
#[php_function(aliases(imagettftext))]
#[allow(clippy::too_many_arguments)]
fn imagefttext(
    req: &mut Request,
    image: Instance<GdImage>,
    size: f64,
    angle: f64,
    x: i64,
    y: i64,
    color: i64,
    font_filename: PhpString,
    text: PhpString,
    #[php(default = [])] options: PhpArray,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let im = im(req, image)?;
    let options = (req.frames().argument_count() > 8).then_some(options);
    ttf(
        req,
        Some(im),
        size,
        angle,
        [x, y, color],
        font_filename,
        text,
        options,
    )
}

/// One of `imagefilter`'s trailing arguments, held to `ty` at its
/// position as the filter's own parse holds it, its `Z_PARAM_LONG`,
/// `Z_PARAM_DOUBLE` or `Z_PARAM_BOOL`, where the stub declares `...$args`
/// untyped.
fn arg(req: &mut Request, args: &[Untyped<Value>], at: usize, ty: &Type) -> Result<Value, Thrown> {
    let value = args.get(at).map_or(Value::null(), |v| v.0);
    let position = u32::try_from(at + 3).unwrap_or(u32::MAX);
    coerce_param(req, &value, ty, position)
}

/// [`arg`] as an `int`, or `default` past the arguments given.
fn long_arg(
    req: &mut Request,
    args: &[Untyped<Value>],
    at: usize,
    default: i64,
) -> Result<i64, Thrown> {
    if at >= args.len() {
        return Ok(default);
    }
    let value = arg(req, args, at, &Type::Int)?;
    to_long(req, &value)
}

/// `imagefilter(GdImage $image, int $filter, ...$args): bool`: two to
/// six arguments, then each filter's own ZPP over all of them.
///
/// ```php
/// $i = imagecreatetruecolor(2, 2); imagefill($i, 0, 0, 0x102030);
/// var_dump(imagefilter($i, IMG_FILTER_NEGATE), dechex(imagecolorat($i, 0, 0)));
/// var_dump(imagefilter($i, IMG_FILTER_BRIGHTNESS, 16), dechex(imagecolorat($i, 0, 0)));
/// var_dump(imagefilter($i, IMG_FILTER_COLORIZE, 1, 2, 3), imagefilter($i, IMG_FILTER_SMOOTH, 1.5));
/// var_dump(imagefilter($i, IMG_FILTER_PIXELATE, 2, true), imagefilter($i, IMG_FILTER_SCATTER, 1, 2, [0x102030]));
/// try { imagefilter($i); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// try { imagefilter($i, IMG_FILTER_BRIGHTNESS); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// try { imagefilter($i, 20); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imagefilter($i, IMG_FILTER_SCATTER, -1, 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(6) "efdfcf"
/// bool(true)
/// string(6) "ffefdf"
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// Wrong parameter count for imagefilter()
/// imagefilter() expects exactly 3 arguments, 2 given
/// imagefilter(): Argument #2 ($filter) must be one of the IMG_FILTER_* filter constants
/// imagefilter(): Argument #3 must be between 0 and 2147483647
/// ```
#[php_function(arity = manual)]
fn imagefilter(
    req: &mut Request,
    image: Raw<Instance<GdImage>>,
    filter: Raw<i64>,
    args: Variadic<Untyped<Value>>,
) -> Result<bool, Thrown> {
    let count = req.frames().argument_count();
    if !(2..=6).contains(&count) {
        return Err(throw_error(
            req,
            "ArgumentCountError",
            "Wrong parameter count for imagefilter()",
        ));
    }
    let image = image.hold(req)?;
    let filter = filter.hold(req)?;
    if !(0..=12).contains(&filter) {
        return Err(arg_value_error(
            req,
            2,
            "be one of the IMG_FILTER_* filter constants",
        ));
    }
    let args = &args.0[..];
    let (min, max) = match filter {
        2 | 3 | 10 => (3, 3),
        4 => (5, 6),
        11 => (3, 4),
        12 => (4, 5),
        _ => (1, count),
    };
    arity(req, min, Some(max))?;
    let im = im(req, image)?;
    let mut ints = [0; 4];
    let mut weight = 0.0;
    match filter {
        2 | 3 => ints[0] = int(long_arg(req, args, 0, 0)?),
        4 => {
            for (i, slot) in ints.iter_mut().enumerate() {
                *slot = int(long_arg(req, args, i, 0)?);
            }
        }
        10 => {
            let value = arg(req, args, 0, &Type::Float)?;
            weight = to_double(req, &value)?;
        }
        11 => {
            ints[0] = int(long_arg(req, args, 0, 0)?);
            if args.len() > 1 {
                let value = arg(req, args, 1, &Type::Bool)?;
                ints[1] = i32::from(matches!(value.as_ref(), ValueRef::Bool(true)));
            }
        }
        12 => return scatter(req, im, args),
        _ => {}
    }
    Ok(gd(req, || im.filter(filter, ints, weight))? == 1)
}

/// `php_image_filter_scatter`: the two amounts, then the colours to
/// scatter where an array of them is given.
fn scatter(req: &mut Request, im: Im, args: &[Untyped<Value>]) -> Result<bool, Thrown> {
    let sub = long_arg(req, args, 0, 0)?;
    let plus = long_arg(req, args, 1, 0)?;
    let colors = if args.len() > 2 {
        let value = arg(req, args, 2, &Type::Array)?;
        value.as_array().copied()
    } else {
        None
    };
    for (position, n) in [(3, sub), (4, plus)] {
        if n < 0 || n > i64::from(i32::MAX) {
            let text = format!("be between 0 and {}", i32::MAX);
            return Err(arg_value_error(req, position, &text));
        }
    }
    let (sub, plus) = (int(sub), int(plus));
    let mut list = Vec::new();
    if let Some(colors) = colors {
        // `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(hash_colors), color)`
        // (`Z/ext/gd/gd.c:3601`), no owner of the table taken: the argument
        // holds it while `zval_get_long` reaches a diagnostic's handler.
        walk::foreach(req, colors, Hold::None, |req, row| -> walk::Step {
            let c = row.value().deref();
            list.push(int(to_long(req, &c)?));
            Ok(ControlFlow::Continue(()))
        })?;
    }
    if list.is_empty() {
        return Ok(gd(req, || im.filter(12, [sub, plus, 0, 0], 0.0))? != 0);
    }
    gd(req, || im.scatter_color(sub, plus, &mut list))
}

/// `imageconvolution(GdImage $image, array $matrix, float $divisor,
/// float $offset): bool`: a 3x3 matrix by integer keys.
///
/// ```php
/// $i = imagecreatetruecolor(3, 3); imagesetpixel($i, 1, 1, 0x808080);
/// var_dump(imageconvolution($i, [[0, 0, 0], [0, 2, 0], [0, 0, 0]], 1, 0), dechex(imagecolorat($i, 1, 1)));
/// try { imageconvolution($i, [[1]], 1, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imageconvolution($i, [[1], [1], [1]], 1, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imageconvolution($i, [['a' => 1, 2, 3], [1, 1, 1], [1, 1, 1]], 1, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imageconvolution($i, [[1, 1, 1], [1, 1, 1], [1, 1, 1]], 0, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(6) "ffffff"
/// imageconvolution(): Argument #2 ($matrix) must be a 3x3 array
/// imageconvolution(): Argument #2 ($matrix) must be a 3x3 array, matrix[0] only has 1 elements
/// imageconvolution(): Argument #2 ($matrix) must be a 3x3 array, matrix[0][2] cannot be found (missing integer key)
/// imageconvolution(): Argument #3 ($divisor) must not be 0
/// ```
#[php_function]
fn imageconvolution(
    req: &mut Request,
    image: Instance<GdImage>,
    matrix: PhpArray,
    divisor: f64,
    offset: f64,
) -> Result<bool, Thrown> {
    let im = im(req, image)?;
    if matrix.len() != 3 {
        return Err(arg_value_error(req, 2, "be a 3x3 array"));
    }
    let mut m = [[0.0f32; 3]; 3];
    for (i, row) in (0_i64..3).zip(m.iter_mut()) {
        let Some(line) = matrix.get(&Key::Int(i)).map(|v| v.deref()) else {
            continue;
        };
        let Some(line) = line.as_array() else {
            continue;
        };
        if line.len() != 3 {
            let text = format!(
                "be a 3x3 array, matrix[{i}] only has {} elements",
                line.len()
            );
            return Err(arg_value_error(req, 2, &text));
        }
        for (j, cell) in (0_i64..3).zip(row.iter_mut()) {
            let Some(value) = line.get(&Key::Int(j)) else {
                let text = format!(
                    "be a 3x3 array, matrix[{i}][{j}] cannot be found (missing integer key)"
                );
                return Err(arg_value_error(req, 2, &text));
            };
            #[allow(clippy::cast_possible_truncation)]
            {
                *cell = to_double(req, &value)? as f32;
            }
        }
    }
    if !divisor.is_finite() {
        return Err(arg_value_error(req, 3, "be finite"));
    }
    #[allow(clippy::cast_possible_truncation)]
    let div = divisor as f32;
    if div == 0.0 {
        return Err(arg_value_error(req, 3, "not be 0"));
    }
    if !offset.is_finite() {
        return Err(arg_value_error(req, 4, "be finite"));
    }
    #[allow(clippy::cast_possible_truncation)]
    let offset = offset as f32;
    gd(req, || im.convolution(m, div, offset))
}

/// `imageflip(GdImage $image, int $mode): true`.
///
/// ```php
/// $i = imagecreatetruecolor(2, 1); imagesetpixel($i, 0, 0, 0xFF);
/// var_dump(imageflip($i, IMG_FLIP_HORIZONTAL), imagecolorat($i, 1, 0));
/// try { imageflip($i, 9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(255)
/// imageflip(): Argument #2 ($mode) must be one of IMG_FLIP_VERTICAL, IMG_FLIP_HORIZONTAL, or IMG_FLIP_BOTH
/// ```
#[php_function]
fn imageflip(req: &mut Request, image: Instance<GdImage>, mode: i64) -> Result<True, Thrown> {
    let im = im(req, image)?;
    if !(1..=3).contains(&mode) {
        return Err(arg_value_error(
            req,
            2,
            "be one of IMG_FLIP_VERTICAL, IMG_FLIP_HORIZONTAL, or IMG_FLIP_BOTH",
        ));
    }
    gd(req, || im.flip(mode))?;
    Ok(True)
}

/// `imageantialias(GdImage $image, bool $enable): true`: a palette image
/// keeps its setting.
///
/// ```php
/// var_dump(imageantialias(imagecreate(1, 1), true), imageantialias(imagecreatetruecolor(1, 1), true));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn imageantialias(
    req: &mut Request,
    image: Instance<GdImage>,
    enable: bool,
) -> Result<True, Thrown> {
    let im = im(req, image)?;
    if im.true_color() {
        im.set_aa(enable);
    }
    Ok(True)
}

/// The `x`, `y`, `width` and `height` keys of a rectangle, each
/// `zval_get_long` cast to the `int` of `gdRect`, the first missing
/// php's `ValueError` at `position`.
fn rect(req: &mut Request, array: PhpArray, position: u32) -> Result<Rect, Thrown> {
    let mut out = [0; 4];
    for (slot, (key, article)) in
        out.iter_mut()
            .zip([("x", "an"), ("y", "a"), ("width", "a"), ("height", "a")])
    {
        let Some(value) = array.find(key.as_bytes()) else {
            let text = format!("have {article} \"{key}\" key");
            return Err(arg_value_error(req, position, &text));
        };
        *slot = int(to_long(req, &value)?);
    }
    let [x, y, width, height] = out;
    Ok(Rect {
        x,
        y,
        width,
        height,
    })
}

/// `imagecrop(GdImage $image, array $rectangle): GdImage|false`.
///
/// ```php
/// $c = imagecrop(imagecreatetruecolor(10, 10), ['x' => 2, 'y' => 2, 'width' => 4, 'height' => 3]);
/// var_dump(imagesx($c), imagesy($c));
/// try { imagecrop(imagecreatetruecolor(1, 1), ['x' => 0]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imagecrop(imagecreatetruecolor(1, 1), ['x' => 2147483647, 'y' => 0, 'width' => 10, 'height' => 1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(4)
/// int(3)
/// imagecrop(): Argument #2 ($rectangle) must have a "y" key
/// imagecrop(): Argument #2 ($rectangle) overflow with "x" and "width" keys
/// ```
#[php_function]
fn imagecrop(
    req: &mut Request,
    image: Instance<GdImage>,
    rectangle: PhpArray,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    let im = im(req, image)?;
    let r = rect(req, rectangle, 2)?;
    let checks = [
        (
            r.width > 0 && r.x > i32::MAX - r.width,
            "overflow with \"x\" and \"width\" keys",
        ),
        (
            r.width < 0 && r.x < i32::MIN - r.width,
            "underflow with \"x\" and \"width\" keys",
        ),
        (
            r.height > 0 && r.y > i32::MAX - r.height,
            "overflow with \"y\" and \"height\" keys",
        ),
        (
            r.height < 0 && r.y < i32::MIN - r.height,
            "underflow with \"y\" and \"height\" keys",
        ),
    ];
    if let Some((_, text)) = checks.iter().find(|(bad, _)| *bad) {
        return Err(argument_error(req, "ValueError", 2, text));
    }
    let cropped = gd(req, || im.crop(r))?;
    wrap(req, cropped)
}

/// `imagecropauto(GdImage $image, int $mode = IMG_CROP_DEFAULT, float
/// $threshold = 0.5, int $color = -1): GdImage|false`.
///
/// ```php
/// $i = imagecreatetruecolor(5, 5); imagefill($i, 0, 0, 0xFFFFFF); imagesetpixel($i, 2, 2, 0);
/// $c = imagecropauto($i, IMG_CROP_WHITE);
/// var_dump(imagesx($c), imagecropauto(imagecreatetruecolor(2, 2), IMG_CROP_BLACK));
/// try { imagecropauto($i, 9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imagecropauto($i, IMG_CROP_THRESHOLD); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// bool(false)
/// imagecropauto(): Argument #2 ($mode) must be a valid mode
/// imagecropauto(): Argument #4 ($color) must be greater than or equal to 0 when using the threshold mode
/// ```
#[php_function]
fn imagecropauto(
    req: &mut Request,
    image: Instance<GdImage>,
    #[php(default = IMG_CROP_DEFAULT)] mode: i64,
    #[php(default = 0.5)] threshold: f64,
    #[php(default = -1)] color: i64,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    let im = im(req, image)?;
    let cropped = match mode {
        0..=4 => gd(req, || im.crop_auto(u32::try_from(mode).unwrap_or(0)))?,
        5 => {
            if color < 0 || (!im.true_color() && color >= i64::from(im.colors_total())) {
                return Err(arg_value_error(
                    req,
                    4,
                    "be greater than or equal to 0 when using the threshold mode",
                ));
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let (color, threshold) = (color as u32, threshold as f32);
            gd(req, || im.crop_threshold(color, threshold))?
        }
        _ => return Err(arg_value_error(req, 2, "be a valid mode")),
    };
    wrap(req, cropped)
}

/// `GD_METHOD_COUNT` of the keg's `gd.h`.
const METHOD_COUNT: i64 = 30;

/// `imagescale(GdImage $image, int $width, int $height = -1, int $mode =
/// IMG_BILINEAR_FIXED): GdImage|false`: a negative side keeps the
/// ratio.
///
/// ```php
/// $i = imagecreatetruecolor(10, 4);
/// $s = imagescale($i, 5);
/// var_dump(imagesx($s), imagesy($s), imagesx(imagescale($i, -1, 8)));
/// try { imagescale($i, -1, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imagescale($i, 5, 5, 99); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imagescale($i, 0, 5); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(5)
/// int(2)
/// int(20)
/// Argument #2 ($width) and argument #3 ($height) cannot be both negative
/// imagescale(): Argument #4 ($mode) must be one of the GD_* constants
/// imagescale(): Argument #2 ($width) must be between 1 and 2147483647
/// ```
#[php_function]
fn imagescale(
    req: &mut Request,
    image: Instance<GdImage>,
    width: i64,
    #[php(default = -1)] height: i64,
    #[php(default = IMG_BILINEAR_FIXED)] mode: i64,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    if !(0..METHOD_COUNT).contains(&mode) {
        return Err(arg_value_error(req, 4, "be one of the GD_* constants"));
    }
    let im = im(req, image)?;
    let (mut w, mut h) = (width, height);
    if h < 0 && w < 0 {
        return Err(throw_error(
            req,
            "ValueError",
            "Argument #2 ($width) and argument #3 ($height) cannot be both negative",
        ));
    }
    let (sx, sy) = (i64::from(im.sx()), i64::from(im.sy()));
    if sx != 0 && h < 0 {
        if w > i64::MAX / sy {
            let text = format!("be less than or equal to {}", i64::MAX / sy);
            return Err(arg_value_error(req, 2, &text));
        }
        h = w * sy / sx;
    }
    if sy != 0 && w < 0 {
        if h > i64::MAX / sx {
            let text = format!("be less than or equal to {}", i64::MAX / sx);
            return Err(arg_value_error(req, 3, &text));
        }
        w = h * sx / sy;
    }
    for (position, n) in [(2, w), (3, h)] {
        if n <= 0 || n > i64::from(i32::MAX) {
            let text = format!("be between 1 and {}", i32::MAX);
            return Err(arg_value_error(req, position, &text));
        }
    }
    let old = im.interpolation_id();
    let (w, h) = (u32::try_from(w).unwrap_or(0), u32::try_from(h).unwrap_or(0));
    let scaled = gd(req, || {
        let scaled = if im.set_interpolation(int(mode)) {
            im.scale(w, h)
        } else {
            None
        };
        im.set_interpolation(old);
        scaled
    })?;
    wrap(req, scaled)
}

/// The six numbers of an affine matrix: integer keys zero to five, each
/// an int, a float or a string read as `zval_get_double`, anything else
/// php's `TypeError` at `position`; a missing one is zero.
fn matrix(
    req: &mut Request,
    array: PhpArray,
    position: u32,
    range: Option<u32>,
) -> Result<[f64; 6], Thrown> {
    let mut m = [0.0; 6];
    for (i, slot) in (0_i64..6).zip(m.iter_mut()) {
        let Some(value) = array.get(&Key::Int(i)).map(|v| v.deref()) else {
            continue;
        };
        match value.as_ref() {
            ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Str(_) => {
                *slot = to_double(req, &value)?;
                if let Some(at) = range
                    && (*slot < f64::from(i32::MIN) || *slot > f64::from(i32::MAX))
                {
                    let text = format!("element {i} must be between {} and {}", i32::MIN, i32::MAX);
                    return Err(argument_error(req, "ValueError", at, &text));
                }
            }
            _ => {
                let text = format!("contains invalid type for element {i}");
                return Err(argument_error(req, "TypeError", position, &text));
            }
        }
    }
    Ok(m)
}

/// `imageaffine(GdImage $image, array $affine, ?array $clip = null):
/// GdImage|false`.
///
/// ```php
/// $i = imagecreatetruecolor(4, 4);
/// $a = imageaffine($i, [2, 0, 0, 2, 0, 0]);
/// var_dump(imagesx($a), imagesx(imageaffine($i, [1, 0, 0, 1, 0, 0], ['x' => 0, 'y' => 0, 'width' => 2, 'height' => 2])));
/// try { imageaffine($i, [1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imageaffine($i, [1, 0, 0, 1, 0, []]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { imageaffine($i, [1, 0, 0, 1, 0, 0], ['x' => 0]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(8)
/// int(2)
/// imageaffine(): Argument #2 ($affine) must have 6 elements
/// imageaffine(): Argument #3 ($clip) contains invalid type for element 5
/// imageaffine(): Argument #3 ($clip) must have a "y" key
/// ```
#[php_function]
fn imageaffine(
    req: &mut Request,
    image: Instance<GdImage>,
    affine: PhpArray,
    #[php(default = null)] clip: Option<PhpArray>,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    let im = im(req, image)?;
    if affine.len() != 6 {
        return Err(arg_value_error(req, 2, "have 6 elements"));
    }
    let m = matrix(req, affine, 3, Some(2))?;
    let area = match clip {
        Some(clip) => Some(rect(req, clip, 3)?),
        None => None,
    };
    let made = gd(req, || im.affine(area, &m))?;
    wrap(req, made)
}

/// `imageaffinematrixget(int $type, $options): array|false`.
///
/// ```php
/// var_dump(imageaffinematrixget(IMG_AFFINE_TRANSLATE, ['x' => 2, 'y' => 3]));
/// echo json_encode(imageaffinematrixget(IMG_AFFINE_ROTATE, 90)), "\n";
/// try { imageaffinematrixget(IMG_AFFINE_SCALE, 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { imageaffinematrixget(IMG_AFFINE_SCALE, ['x' => 1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imageaffinematrixget(9, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(6) {
///   [0]=>
///   float(1)
///   [1]=>
///   float(0)
///   [2]=>
///   float(0)
///   [3]=>
///   float(1)
///   [4]=>
///   float(2)
///   [5]=>
///   float(3)
/// }
/// [6.123233995736766e-17,1,-1,6.123233995736766e-17,0,0]
/// imageaffinematrixget(): Argument #1 ($type) must be of type array when using translate or scale
/// imageaffinematrixget(): Argument #2 ($options) must have a "y" key
/// imageaffinematrixget(): Argument #1 ($type) must be a valid element type
/// ```
#[php_function]
fn imageaffinematrixget(
    req: &mut Request,
    r#type: i64,
    options: Untyped<Value>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let options = options.0;
    let (x, y) = match r#type {
        0 | 1 => {
            let Some(array) = options.as_array() else {
                return Err(arg_type_error(
                    req,
                    1,
                    "be of type array when using translate or scale",
                ));
            };
            let mut xy = [0.0; 2];
            for (slot, (key, article)) in xy.iter_mut().zip([("x", "an"), ("y", "a")]) {
                let Some(value) = array.find(key.as_bytes()) else {
                    let text = format!("have {article} \"{key}\" key");
                    return Err(arg_value_error(req, 2, &text));
                };
                *slot = to_double(req, &value)?;
            }
            (xy[0], xy[1])
        }
        2..=4 => (to_double(req, &options)?, 0.0),
        _ => return Err(arg_value_error(req, 1, "be a valid element type")),
    };
    match affine_matrix(r#type, x, y) {
        Some(m) => Ok(OrFalse::Value(crate::list(req, &m.map(Value::float))?)),
        None => Ok(OrFalse::False),
    }
}

/// `imageaffinematrixconcat(array $matrix1, array $matrix2):
/// array|false`.
///
/// ```php
/// $m = imageaffinematrixconcat([1, 0, 0, 1, 2, 3], [2, 0, 0, 2, 0, 0]);
/// echo json_encode($m), "\n";
/// try { imageaffinematrixconcat([1], [1, 0, 0, 1, 0, 0]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imageaffinematrixconcat([1, 0, 0, 1, 0, 0], [1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imageaffinematrixconcat([1, 0, 0, 1, 0, 0], [1, 0, 0, 1, 0, null]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// [2,0,0,2,4,6]
/// imageaffinematrixconcat(): Argument #1 ($matrix1) must have 6 elements
/// imageaffinematrixconcat(): Argument #1 ($matrix1) must have 6 elements
/// imageaffinematrixconcat(): Argument #2 ($matrix2) contains invalid type for element 5
/// ```
#[php_function]
fn imageaffinematrixconcat(
    req: &mut Request,
    matrix1: PhpArray,
    matrix2: PhpArray,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    if matrix1.len() != 6 || matrix2.len() != 6 {
        return Err(arg_value_error(req, 1, "have 6 elements"));
    }
    let mut m1 = [0.0; 6];
    let mut m2 = [0.0; 6];
    for i in 0..6 {
        for (array, out, position) in [(&matrix1, &mut m1, 1), (&matrix2, &mut m2, 2)] {
            let Some(value) = array.get(&Key::Int(i)).map(|v| v.deref()) else {
                continue;
            };
            if !matches!(
                value.as_ref(),
                ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Str(_)
            ) {
                let text = format!("contains invalid type for element {i}");
                return Err(argument_error(req, "TypeError", position, &text));
            }
            out[usize::try_from(i).unwrap_or(0)] = to_double(req, &value)?;
        }
    }
    match affine_concat(&m1, &m2) {
        Some(m) => Ok(OrFalse::Value(crate::list(req, &m.map(Value::float))?)),
        None => Ok(OrFalse::False),
    }
}

/// `imagegetinterpolation(GdImage $image): int`.
///
/// ```php
/// $i = imagecreatetruecolor(1, 1);
/// var_dump(imagegetinterpolation($i));
/// imagesetinterpolation($i, IMG_BICUBIC);
/// var_dump(imagegetinterpolation($i));
/// ```
/// ```expect
/// int(3)
/// int(3)
/// ```
#[php_function]
fn imagegetinterpolation(req: &mut Request, image: Instance<GdImage>) -> Result<i64, Thrown> {
    let im = im(req, image)?;
    Ok(i64::from(im.interpolation()))
}

/// `imagesetinterpolation(GdImage $image, int $method =
/// IMG_BILINEAR_FIXED): bool`: `-1` is the default.
///
/// ```php
/// $i = imagecreatetruecolor(1, 1);
/// var_dump(imagesetinterpolation($i, -1), imagegetinterpolation($i), imagesetinterpolation($i, 99));
/// ```
/// ```expect
/// bool(true)
/// int(3)
/// bool(false)
/// ```
#[php_function]
fn imagesetinterpolation(
    req: &mut Request,
    image: Instance<GdImage>,
    #[php(default = IMG_BILINEAR_FIXED)] method: i64,
) -> Result<bool, Thrown> {
    let im = im(req, image)?;
    let method = if method == -1 { 3 } else { method };
    gd(req, || im.set_interpolation(int(method)))
}

/// `imageresolution(GdImage $image, ?int $resolution_x = null, ?int
/// $resolution_y = null): array|true`: the DPI read, or set from one or
/// both.
///
/// ```php
/// $i = imagecreatetruecolor(1, 1);
/// var_dump(imageresolution($i), imageresolution($i, 300), imageresolution($i), imageresolution($i, null, 72), imageresolution($i));
/// try { imageresolution($i, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(96)
///   [1]=>
///   int(96)
/// }
/// bool(true)
/// array(2) {
///   [0]=>
///   int(300)
///   [1]=>
///   int(300)
/// }
/// bool(true)
/// array(2) {
///   [0]=>
///   int(72)
///   [1]=>
///   int(72)
/// }
/// imageresolution(): Argument #2 ($resolution_x) must be between 0 and 4294967295
/// ```
#[php_function]
fn imageresolution(
    req: &mut Request,
    image: Instance<GdImage>,
    #[php(default = null)] resolution_x: Option<i64>,
    #[php(default = null)] resolution_y: Option<i64>,
) -> Result<Union<Owner<PhpArray>, True>, Thrown> {
    let im = im(req, image)?;
    let check = |req: &mut Request, n: i64, position: u32| {
        if n < 0 || n > i64::from(u32::MAX) {
            let text = format!("be between 0 and {}", u32::MAX);
            return Err(arg_value_error(req, position, &text));
        }
        Ok(u32::try_from(n).unwrap_or(0))
    };
    let (x, y) = match (resolution_x, resolution_y) {
        (Some(x), Some(y)) => (check(req, x, 2)?, check(req, y, 3)?),
        (Some(x), None) => {
            let x = check(req, x, 2)?;
            (x, x)
        }
        (None, Some(y)) => {
            let y = check(req, y, 3)?;
            (y, y)
        }
        (None, None) => {
            let (x, y) = im.resolution();
            let pair = [Value::int(i64::from(x)), Value::int(i64::from(y))];
            return Ok(Union::A(crate::list(req, &pair)?));
        }
    };
    im.set_resolution(x, y);
    Ok(Union::B(True))
}
