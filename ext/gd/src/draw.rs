//! The palette, the pixels and the drawing primitives: `gd.c`'s natives
//! that hand one image and its numbers to libgd and answer `true`, the
//! colour lookups, and the bitmap-font text.

use std::ops::ControlFlow;

use php_ext::abi::{False, Instance, OrFalse, True, Union};
use php_ext::attr::php_function;
use php_ext::coerce::to_long;
use php_ext::raising::{Level, Thrown, arg_value_error, argument_error, docref, value_error};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value};

use crate::ffi::{ColorOp, Copy, Draw, Font, Point};
use crate::image::{GdFont, GdImage, gd, im, int, with_font};

/// `imagesetstyle(GdImage $image, array $style): bool`: each element
/// read as an `int`, `zval_get_long`.
///
/// ```php
/// $i = imagecreatetruecolor(6, 1);
/// var_dump(imagesetstyle($i, [0xFF0000, 0xFF0000, "255"]));
/// imageline($i, 0, 0, 5, 0, IMG_COLOR_STYLED);
/// echo dechex(imagecolorat($i, 1, 0)), ' ', dechex(imagecolorat($i, 2, 0)), "\n";
/// try { imagesetstyle($i, []); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// ff0000 ff
/// imagesetstyle(): Argument #2 ($style) must not be empty
/// ```
#[php_function]
fn imagesetstyle(
    req: &mut Request,
    image: Instance<GdImage>,
    style: PhpArray,
) -> Result<bool, Thrown> {
    let im = im(req, image)?;
    if style.is_empty() {
        return Err(arg_value_error(req, 2, "not be empty"));
    }
    let mut styles = Vec::with_capacity(style.len());
    // `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(styles), item)`
    // (`Z/ext/gd/gd.c:642`), no owner of the table taken: the argument
    // holds it while `zval_get_long` reaches a diagnostic's handler.
    walk::foreach(req, style, Hold::None, |req, row| -> walk::Step {
        let item = row.value().deref();
        styles.push(int(to_long(req, &item)?));
        Ok(ControlFlow::Continue(()))
    })?;
    im.set_style(&mut styles);
    Ok(true)
}

/// `imagetruecolortopalette(GdImage $image, bool $dither, int
/// $num_colors): bool`.
///
/// ```php
/// $i = imagecreatetruecolor(2, 2);
/// imagesetpixel($i, 1, 1, 0x00FF00);
/// var_dump(imagetruecolortopalette($i, false, 4), imageistruecolor($i), imagecolorstotal($i));
/// try { imagetruecolortopalette($i, false, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// int(2)
/// imagetruecolortopalette(): Argument #3 ($num_colors) must be greater than 0 and less than 2147483647
/// ```
#[php_function]
fn imagetruecolortopalette(
    req: &mut Request,
    image: Instance<GdImage>,
    dither: bool,
    num_colors: i64,
) -> Result<bool, Thrown> {
    let im = im(req, image)?;
    if num_colors <= 0 || num_colors > i64::from(i32::MAX) {
        let text = format!("be greater than 0 and less than {}", i32::MAX);
        return Err(arg_value_error(req, 3, &text));
    }
    if gd(req, || im.true_color_to_palette(dither, int(num_colors)))? {
        return Ok(true);
    }
    docref(req, Level::Warning, "Couldn't convert to palette");
    Ok(false)
}

/// `imagepalettetotruecolor(GdImage $image): bool`.
///
/// ```php
/// $i = imagecreate(2, 2);
/// imagecolorallocate($i, 1, 2, 3);
/// var_dump(imagepalettetotruecolor($i), imageistruecolor($i), dechex(imagecolorat($i, 0, 0)));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(5) "10203"
/// ```
#[php_function]
fn imagepalettetotruecolor(req: &mut Request, image: Instance<GdImage>) -> Result<bool, Thrown> {
    let im = im(req, image)?;
    gd(req, || im.palette_to_true_color())
}

/// `imagecolormatch(GdImage $image1, GdImage $image2): true`: libgd's
/// refusals as php words them.
///
/// ```php
/// $t = imagecreatetruecolor(2, 2); $p = imagecreate(2, 2);
/// imagecolorallocate($p, 0, 0, 0);
/// var_dump(imagecolormatch($t, $p));
/// foreach ([[$p, $p], [$t, $t], [$t, imagecreate(3, 3)], [$t, imagecreate(2, 2)]] as [$a, $b]) {
///     try { imagecolormatch($a, $b); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// bool(true)
/// imagecolormatch(): Argument #1 ($image1) must be TrueColor
/// imagecolormatch(): Argument #2 ($image2) must be Palette
/// imagecolormatch(): Argument #2 ($image2) must be the same size as argument #1 ($im1)
/// imagecolormatch(): Argument #2 ($image2) must have at least one color
/// ```
#[php_function]
fn imagecolormatch(
    req: &mut Request,
    image1: Instance<GdImage>,
    image2: Instance<GdImage>,
) -> Result<True, Thrown> {
    let (a, b) = (im(req, image1)?, im(req, image2)?);
    let (position, text) = match gd(req, || a.color_match(b))? {
        -1 => (1, "be TrueColor"),
        -2 => (2, "be Palette"),
        -3 => (2, "be the same size as argument #1 ($im1)"),
        -4 => (2, "have at least one color"),
        _ => return Ok(True),
    };
    Err(arg_value_error(req, position, text))
}

/// `imagesetthickness(GdImage $image, int $thickness): true`.
///
/// ```php
/// $i = imagecreatetruecolor(5, 5);
/// var_dump(imagesetthickness($i, 3));
/// imageline($i, 0, 2, 4, 2, 0xFFFFFF);
/// echo dechex(imagecolorat($i, 2, 1)), ' ', dechex(imagecolorat($i, 2, 4)), "\n";
/// ```
/// ```expect
/// bool(true)
/// ffffff 0
/// ```
#[php_function]
fn imagesetthickness(
    req: &mut Request,
    image: Instance<GdImage>,
    thickness: i64,
) -> Result<True, Thrown> {
    im(req, image)?.set_thickness(int(thickness));
    Ok(True)
}

/// `imagefilledellipse(GdImage $image, int $center_x, int $center_y, int
/// $width, int $height, int $color): true`.
///
/// ```php
/// $i = imagecreatetruecolor(9, 9);
/// var_dump(imagefilledellipse($i, 4, 4, 6, 6, 0xFF));
/// echo dechex(imagecolorat($i, 4, 4)), ' ', dechex(imagecolorat($i, 0, 0)), "\n";
/// try { imagefilledellipse($i, 4, 4, -1, 6, 0xFF); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// ff 0
/// imagefilledellipse(): Argument #4 ($width) must be between 0 and 2147483647
/// ```
#[php_function]
#[allow(clippy::too_many_arguments)]
fn imagefilledellipse(
    req: &mut Request,
    image: Instance<GdImage>,
    center_x: i64,
    center_y: i64,
    width: i64,
    height: i64,
    color: i64,
) -> Result<True, Thrown> {
    if width < 0 || width > i64::from(i32::MAX) {
        let text = format!("be between 0 and {}", i32::MAX);
        return Err(arg_value_error(req, 4, &text));
    }
    let im = im(req, image)?;
    let at = [center_x, center_y, width, height].map(int);
    gd(req, || im.draw(Draw::FilledEllipse, at, int(color)))?;
    Ok(True)
}

/// An angle as `imagearc` and `imagefilledarc` take it: a negative one
/// reduced by `%= 360`, C's remainder.
fn angle(n: i64) -> i32 {
    let n = int(n);
    if n < 0 { n % 360 } else { n }
}

/// `imagefilledarc(GdImage $image, int $center_x, int $center_y, int
/// $width, int $height, int $start_angle, int $end_angle, int $color,
/// int $style): true`.
///
/// ```php
/// $i = imagecreatetruecolor(9, 9);
/// var_dump(imagefilledarc($i, 4, 4, 8, 8, 0, 90, 0xFF00, IMG_ARC_PIE));
/// echo dechex(imagecolorat($i, 6, 6)), ' ', dechex(imagecolorat($i, 2, 2)), "\n";
/// ```
/// ```expect
/// bool(true)
/// ff00 0
/// ```
#[php_function]
#[allow(clippy::too_many_arguments)]
fn imagefilledarc(
    req: &mut Request,
    image: Instance<GdImage>,
    center_x: i64,
    center_y: i64,
    width: i64,
    height: i64,
    start_angle: i64,
    end_angle: i64,
    color: i64,
    style: i64,
) -> Result<True, Thrown> {
    let im = im(req, image)?;
    let at = [center_x, center_y, width, height].map(int);
    let arc = [
        at[0],
        at[1],
        at[2],
        at[3],
        angle(start_angle),
        angle(end_angle),
    ];
    gd(req, || im.arc(arc, int(color), Some(int(style))))?;
    Ok(True)
}

/// `imagealphablending(GdImage $image, bool $enable): true`.
///
/// ```php
/// $i = imagecreatetruecolor(1, 1);
/// var_dump(imagealphablending($i, false));
/// imagesetpixel($i, 0, 0, 0x7F00FF00);
/// echo dechex(imagecolorat($i, 0, 0)), "\n";
/// ```
/// ```expect
/// bool(true)
/// 7f00ff00
/// ```
#[php_function]
fn imagealphablending(
    req: &mut Request,
    image: Instance<GdImage>,
    enable: bool,
) -> Result<True, Thrown> {
    im(req, image)?.alpha_blending(enable.into());
    Ok(True)
}

/// `imagesavealpha(GdImage $image, bool $enable): true`.
///
/// ```php
/// var_dump(imagesavealpha(imagecreatetruecolor(1, 1), true));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn imagesavealpha(
    req: &mut Request,
    image: Instance<GdImage>,
    enable: bool,
) -> Result<True, Thrown> {
    im(req, image)?.save_alpha(enable);
    Ok(True)
}

/// `imagelayereffect(GdImage $image, int $effect): true`: the effect is
/// libgd's blending setting.
///
/// ```php
/// $i = imagecreatetruecolor(1, 1);
/// imagesetpixel($i, 0, 0, 0x808080);
/// var_dump(imagelayereffect($i, IMG_EFFECT_MULTIPLY));
/// imagesetpixel($i, 0, 0, 0x808080);
/// echo dechex(imagecolorat($i, 0, 0)), "\n";
/// ```
/// ```expect
/// bool(true)
/// 404040
/// ```
#[php_function]
fn imagelayereffect(
    req: &mut Request,
    image: Instance<GdImage>,
    effect: i64,
) -> Result<True, Thrown> {
    im(req, image)?.alpha_blending(int(effect));
    Ok(True)
}

/// `CHECK_RGBA_RANGE`: each channel in its range, the first out of it
/// php's `ValueError` at its position.
fn rgba(req: &mut Request, channels: &[i64], first: u32) -> Result<[i32; 4], Thrown> {
    let mut out = [0; 4];
    for (i, n) in channels.iter().enumerate() {
        let max = if i == 3 { 127 } else { 255 };
        if *n < 0 || *n > max {
            let text = format!("be between 0 and {max} (inclusive)");
            let position = first + u32::try_from(i).unwrap_or(0);
            return Err(arg_value_error(req, position, &text));
        }
        out[i] = int(*n);
    }
    Ok(out)
}

/// One of the colour natives over the image and checked channels.
fn color(
    req: &mut Request,
    image: Instance<GdImage>,
    op: ColorOp,
    channels: &[i64],
) -> Result<i64, Thrown> {
    let im = im(req, image)?;
    let channels = rgba(req, channels, 2)?;
    Ok(i64::from(gd(req, || im.color(op, channels))?))
}

/// `imagecolorallocatealpha(GdImage $image, int $red, int $green, int
/// $blue, int $alpha): int|false`.
///
/// ```php
/// $t = imagecreatetruecolor(1, 1); $p = imagecreate(1, 1);
/// var_dump(imagecolorallocatealpha($t, 255, 0, 0, 64), imagecolorallocatealpha($p, 255, 0, 0, 64));
/// try { imagecolorallocatealpha($t, 0, 0, 0, 128); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// for ($n = 0; $n < 256; $n++) imagecolorallocatealpha($p, $n, 0, 0, 0);
/// var_dump(imagecolorallocatealpha($p, 0, 1, 0, 0));
/// ```
/// ```expect
/// int(1090453504)
/// int(0)
/// imagecolorallocatealpha(): Argument #5 ($alpha) must be between 0 and 127 (inclusive)
/// bool(false)
/// ```
#[php_function]
fn imagecolorallocatealpha(
    req: &mut Request,
    image: Instance<GdImage>,
    red: i64,
    green: i64,
    blue: i64,
    alpha: i64,
) -> Result<OrFalse<i64>, Thrown> {
    let c = color(
        req,
        image,
        ColorOp::AllocateAlpha,
        &[red, green, blue, alpha],
    )?;
    Ok(if c < 0 {
        OrFalse::False
    } else {
        OrFalse::Value(c)
    })
}

/// `imagecolorresolvealpha(GdImage $image, int $red, int $green, int
/// $blue, int $alpha): int`.
///
/// ```php
/// $p = imagecreate(1, 1);
/// var_dump(imagecolorresolvealpha($p, 1, 2, 3, 4), imagecolorresolvealpha($p, 1, 2, 3, 4), imagecolorresolvealpha($p, 9, 9, 9, 9));
/// ```
/// ```expect
/// int(0)
/// int(0)
/// int(1)
/// ```
#[php_function]
fn imagecolorresolvealpha(
    req: &mut Request,
    image: Instance<GdImage>,
    red: i64,
    green: i64,
    blue: i64,
    alpha: i64,
) -> Result<i64, Thrown> {
    color(req, image, ColorOp::Resolve, &[red, green, blue, alpha])
}

/// `imagecolorclosestalpha(GdImage $image, int $red, int $green, int
/// $blue, int $alpha): int`.
///
/// ```php
/// $p = imagecreate(1, 1);
/// var_dump(imagecolorclosestalpha($p, 1, 2, 3, 4));
/// imagecolorallocatealpha($p, 0, 0, 0, 0); imagecolorallocatealpha($p, 200, 200, 200, 100);
/// var_dump(imagecolorclosestalpha($p, 190, 190, 190, 90));
/// ```
/// ```expect
/// int(-1)
/// int(1)
/// ```
#[php_function]
fn imagecolorclosestalpha(
    req: &mut Request,
    image: Instance<GdImage>,
    red: i64,
    green: i64,
    blue: i64,
    alpha: i64,
) -> Result<i64, Thrown> {
    color(req, image, ColorOp::Closest, &[red, green, blue, alpha])
}

/// `imagecolorexactalpha(GdImage $image, int $red, int $green, int
/// $blue, int $alpha): int`.
///
/// ```php
/// $p = imagecreate(1, 1);
/// imagecolorallocatealpha($p, 1, 2, 3, 4);
/// var_dump(imagecolorexactalpha($p, 1, 2, 3, 4), imagecolorexactalpha($p, 1, 2, 3, 5));
/// var_dump(imagecolorexactalpha(imagecreatetruecolor(1, 1), 1, 2, 3, 4));
/// ```
/// ```expect
/// int(0)
/// int(-1)
/// int(67174915)
/// ```
#[php_function]
fn imagecolorexactalpha(
    req: &mut Request,
    image: Instance<GdImage>,
    red: i64,
    green: i64,
    blue: i64,
    alpha: i64,
) -> Result<i64, Thrown> {
    color(req, image, ColorOp::Exact, &[red, green, blue, alpha])
}

/// One of the copies over two images, each number cast to a C `int`.
fn copy(
    req: &mut Request,
    dst: Instance<GdImage>,
    src: Instance<GdImage>,
    op: Copy,
    at: [i64; 4],
    size: [i64; 4],
) -> Result<True, Thrown> {
    let (d, s) = (im(req, dst)?, im(req, src)?);
    gd(req, || d.copy(s, op, at.map(int), size.map(int)))?;
    Ok(True)
}

/// `imagecopyresampled(GdImage $dst_image, GdImage $src_image, int
/// $dst_x, int $dst_y, int $src_x, int $src_y, int $dst_width, int
/// $dst_height, int $src_width, int $src_height): true`.
///
/// ```php
/// $s = imagecreatetruecolor(2, 2); imagefill($s, 0, 0, 0xFF0000);
/// $d = imagecreatetruecolor(4, 4);
/// var_dump(imagecopyresampled($d, $s, 0, 0, 0, 0, 4, 4, 2, 2));
/// echo dechex(imagecolorat($d, 3, 3)), "\n";
/// ```
/// ```expect
/// bool(true)
/// ff0000
/// ```
#[php_function]
#[allow(clippy::too_many_arguments)]
fn imagecopyresampled(
    req: &mut Request,
    dst_image: Instance<GdImage>,
    src_image: Instance<GdImage>,
    dst_x: i64,
    dst_y: i64,
    src_x: i64,
    src_y: i64,
    dst_width: i64,
    dst_height: i64,
    src_width: i64,
    src_height: i64,
) -> Result<True, Thrown> {
    let at = [dst_x, dst_y, src_x, src_y];
    let size = [dst_width, dst_height, src_width, src_height];
    copy(req, dst_image, src_image, Copy::Resampled, at, size)
}

/// `imagesettile(GdImage $image, GdImage $tile): true`.
///
/// ```php
/// $t = imagecreatetruecolor(1, 1); imagesetpixel($t, 0, 0, 0xABCDEF);
/// $i = imagecreatetruecolor(3, 3);
/// var_dump(imagesettile($i, $t));
/// imagefilledrectangle($i, 0, 0, 2, 2, IMG_COLOR_TILED);
/// echo dechex(imagecolorat($i, 2, 2)), "\n";
/// ```
/// ```expect
/// bool(true)
/// abcdef
/// ```
#[php_function]
fn imagesettile(
    req: &mut Request,
    image: Instance<GdImage>,
    tile: Instance<GdImage>,
) -> Result<True, Thrown> {
    let (i, t) = (im(req, image)?, im(req, tile)?);
    i.set_tile(t);
    Ok(True)
}

/// `imagesetbrush(GdImage $image, GdImage $brush): true`.
///
/// ```php
/// $b = imagecreatetruecolor(1, 1); imagesetpixel($b, 0, 0, 0x123456);
/// $i = imagecreatetruecolor(3, 3);
/// var_dump(imagesetbrush($i, $b));
/// imageline($i, 0, 1, 2, 1, IMG_COLOR_BRUSHED);
/// echo dechex(imagecolorat($i, 1, 1)), "\n";
/// ```
/// ```expect
/// bool(true)
/// 123456
/// ```
#[php_function]
fn imagesetbrush(
    req: &mut Request,
    image: Instance<GdImage>,
    brush: Instance<GdImage>,
) -> Result<True, Thrown> {
    let (i, b) = (im(req, image)?, im(req, brush)?);
    i.set_brush(b);
    Ok(True)
}

/// `imagecolorallocate(GdImage $image, int $red, int $green, int $blue):
/// int|false`.
///
/// ```php
/// $p = imagecreate(1, 1);
/// var_dump(imagecolorallocate($p, 1, 2, 3), imagecolorallocate($p, 1, 2, 3), imagecolorallocate(imagecreatetruecolor(1, 1), 1, 2, 3));
/// try { imagecolorallocate($p, 256, 0, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(0)
/// int(1)
/// int(66051)
/// imagecolorallocate(): Argument #2 ($red) must be between 0 and 255 (inclusive)
/// ```
#[php_function]
fn imagecolorallocate(
    req: &mut Request,
    image: Instance<GdImage>,
    red: i64,
    green: i64,
    blue: i64,
) -> Result<OrFalse<i64>, Thrown> {
    let c = color(req, image, ColorOp::Allocate, &[red, green, blue])?;
    Ok(if c < 0 {
        OrFalse::False
    } else {
        OrFalse::Value(c)
    })
}

/// `imagepalettecopy(GdImage $dst, GdImage $src): void`.
///
/// ```php
/// $s = imagecreate(1, 1); imagecolorallocate($s, 9, 8, 7);
/// $d = imagecreate(1, 1);
/// var_dump(imagepalettecopy($d, $s), imagecolorstotal($d), imagecolorsforindex($d, 0)['red']);
/// ```
/// ```expect
/// NULL
/// int(1)
/// int(9)
/// ```
#[php_function]
fn imagepalettecopy(
    req: &mut Request,
    dst: Instance<GdImage>,
    src: Instance<GdImage>,
) -> Result<(), Thrown> {
    let (d, s) = (im(req, dst)?, im(req, src)?);
    gd(req, || d.palette_copy(s))?;
    Ok(())
}

/// `imagecolorat(GdImage $image, int $x, int $y): int|false`: out of
/// bounds is php's notice.
///
/// ```php
/// $i = imagecreatetruecolor(2, 2); imagesetpixel($i, 1, 1, 0x112233);
/// var_dump(imagecolorat($i, 1, 1), imagecolorat($i, 2, 0));
/// $p = imagecreate(2, 2); imagecolorallocate($p, 0, 0, 0); imagecolorallocate($p, 1, 1, 1);
/// imagesetpixel($p, 0, 1, 1);
/// var_dump(imagecolorat($p, 0, 1), imagecolorat($p, -1, 0));
/// ```
/// ```expect
/// Notice: imagecolorat(): 2,0 is out of bounds in %s on line 3
/// int(1122867)
/// bool(false)
///
/// Notice: imagecolorat(): -1,0 is out of bounds in %s on line 6
/// int(1)
/// bool(false)
/// ```
#[php_function]
fn imagecolorat(
    req: &mut Request,
    image: Instance<GdImage>,
    x: i64,
    y: i64,
) -> Result<OrFalse<i64>, Thrown> {
    let im = im(req, image)?;
    if let Some(c) = im.pixel_at(int(x), int(y)) {
        return Ok(OrFalse::Value(i64::from(c)));
    }
    docref(req, Level::Notice, &format!("{x},{y} is out of bounds"));
    Ok(OrFalse::False)
}

/// `imagecolorclosest(GdImage $image, int $red, int $green, int $blue):
/// int`.
///
/// ```php
/// $p = imagecreate(1, 1); imagecolorallocate($p, 0, 0, 0); imagecolorallocate($p, 250, 250, 250);
/// var_dump(imagecolorclosest($p, 200, 200, 200), imagecolorclosest(imagecreatetruecolor(1, 1), 1, 2, 3));
/// ```
/// ```expect
/// int(1)
/// int(66051)
/// ```
#[php_function]
fn imagecolorclosest(
    req: &mut Request,
    image: Instance<GdImage>,
    red: i64,
    green: i64,
    blue: i64,
) -> Result<i64, Thrown> {
    color(req, image, ColorOp::Closest, &[red, green, blue])
}

/// `imagecolorclosesthwb(GdImage $image, int $red, int $green, int
/// $blue): int`.
///
/// ```php
/// $p = imagecreate(1, 1); imagecolorallocate($p, 255, 0, 0); imagecolorallocate($p, 0, 0, 255);
/// var_dump(imagecolorclosesthwb($p, 0, 0, 200));
/// ```
/// ```expect
/// int(1)
/// ```
#[php_function]
fn imagecolorclosesthwb(
    req: &mut Request,
    image: Instance<GdImage>,
    red: i64,
    green: i64,
    blue: i64,
) -> Result<i64, Thrown> {
    color(req, image, ColorOp::Hwb, &[red, green, blue])
}

/// `imagecolordeallocate(GdImage $image, int $color): true`: nothing to
/// do on a true color image, an index in the palette otherwise.
///
/// ```php
/// $p = imagecreate(1, 1); imagecolorallocate($p, 1, 1, 1);
/// var_dump(imagecolordeallocate($p, 0), imagecolordeallocate(imagecreatetruecolor(1, 1), 99));
/// try { imagecolordeallocate($p, 5); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// imagecolordeallocate(): Argument #2 ($color) must be between 0 and 1
/// ```
#[php_function]
fn imagecolordeallocate(
    req: &mut Request,
    image: Instance<GdImage>,
    color: i64,
) -> Result<True, Thrown> {
    let im = im(req, image)?;
    if im.true_color() {
        return Ok(True);
    }
    let col = int(color);
    if col >= 0 && col < im.colors_total() {
        im.color_deallocate(col);
        return Ok(True);
    }
    let text = format!("be between 0 and {}", im.colors_total());
    Err(arg_value_error(req, 2, &text))
}

/// `imagecolorresolve(GdImage $image, int $red, int $green, int $blue):
/// int`.
///
/// ```php
/// $p = imagecreate(1, 1);
/// var_dump(imagecolorresolve($p, 4, 5, 6), imagecolorresolve($p, 4, 5, 6), imagecolorresolve(imagecreatetruecolor(1, 1), 4, 5, 6));
/// ```
/// ```expect
/// int(0)
/// int(0)
/// int(263430)
/// ```
#[php_function]
fn imagecolorresolve(
    req: &mut Request,
    image: Instance<GdImage>,
    red: i64,
    green: i64,
    blue: i64,
) -> Result<i64, Thrown> {
    color(req, image, ColorOp::Resolve, &[red, green, blue])
}

/// `imagecolorexact(GdImage $image, int $red, int $green, int $blue):
/// int`.
///
/// ```php
/// $p = imagecreate(1, 1); imagecolorallocate($p, 4, 5, 6);
/// var_dump(imagecolorexact($p, 4, 5, 6), imagecolorexact($p, 4, 5, 7));
/// ```
/// ```expect
/// int(0)
/// int(-1)
/// ```
#[php_function]
fn imagecolorexact(
    req: &mut Request,
    image: Instance<GdImage>,
    red: i64,
    green: i64,
    blue: i64,
) -> Result<i64, Thrown> {
    color(req, image, ColorOp::Exact, &[red, green, blue])
}

/// `imagecolorset(GdImage $image, int $color, int $red, int $green, int
/// $blue, int $alpha = 0): false|null`: a palette entry written in
/// place.
///
/// ```php
/// $p = imagecreate(1, 1); imagecolorallocate($p, 1, 1, 1);
/// var_dump(imagecolorset($p, 0, 9, 8, 7, 6), imagecolorsforindex($p, 0), imagecolorset($p, 3, 1, 1, 1));
/// try { imagecolorset($p, 0, 1, 1, 1, 128); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// NULL
/// array(4) {
///   ["red"]=>
///   int(9)
///   ["green"]=>
///   int(8)
///   ["blue"]=>
///   int(7)
///   ["alpha"]=>
///   int(6)
/// }
/// bool(false)
/// imagecolorset(): Argument #6 ($alpha) must be between 0 and 127 (inclusive)
/// ```
#[php_function]
#[allow(clippy::too_many_arguments)]
fn imagecolorset(
    req: &mut Request,
    image: Instance<GdImage>,
    color: i64,
    red: i64,
    green: i64,
    blue: i64,
    #[php(default = 0)] alpha: i64,
) -> Result<Option<False>, Thrown> {
    let im = im(req, image)?;
    let channels = rgba(req, &[red, green, blue, alpha], 3)?;
    let col = int(color);
    if col >= 0 && col < im.colors_total() {
        im.set_palette(col, channels);
        return Ok(None);
    }
    Ok(Some(False))
}

/// `imagecolorsforindex(GdImage $image, int $color): array`: any
/// non-negative colour of a true color image, an index of the palette
/// otherwise.
///
/// ```php
/// var_dump(imagecolorsforindex(imagecreatetruecolor(1, 1), 0x7F102030));
/// try { imagecolorsforindex(imagecreate(1, 1), 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(4) {
///   ["red"]=>
///   int(16)
///   ["green"]=>
///   int(32)
///   ["blue"]=>
///   int(48)
///   ["alpha"]=>
///   int(127)
/// }
/// imagecolorsforindex(): Argument #2 ($color) is out of range
/// ```
#[php_function]
fn imagecolorsforindex(
    req: &mut Request,
    image: Instance<GdImage>,
    color: i64,
) -> Result<Owner<PhpArray>, Thrown> {
    let im = im(req, image)?;
    let col = int(color);
    if col < 0 || (!im.true_color() && col >= im.colors_total()) {
        return Err(argument_error(req, "ValueError", 2, "is out of range"));
    }
    let channels = im.rgba(col);
    let mut array = PhpArray::new(req)?;
    for (name, n) in ["red", "green", "blue", "alpha"].into_iter().zip(channels) {
        crate::set(req, &mut array, name, Owner::int(i64::from(n)))?;
    }
    Ok(array)
}

/// One channel through `pow(c / 255.0, gamma) * 255 + .5`, cast as C
/// casts it.
#[allow(clippy::cast_possible_truncation)]
fn gamma(c: i32, gamma: f64) -> i32 {
    ((f64::from(c) / 255.0).powf(gamma) * 255.0 + 0.5) as i32
}

/// `imagegammacorrect(GdImage $image, float $input_gamma,`
/// `float $output_gamma): true`.
///
/// ```php
/// $i = imagecreatetruecolor(1, 1); imagesetpixel($i, 0, 0, 0x404040);
/// var_dump(imagegammacorrect($i, 1.0, 2.2));
/// echo dechex(imagecolorat($i, 0, 0)), "\n";
/// $p = imagecreate(1, 1); imagecolorallocate($p, 64, 64, 64);
/// imagegammacorrect($p, 2.2, 1.0);
/// var_dump(imagecolorsforindex($p, 0)['red']);
/// try { imagegammacorrect($i, 0, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imagegammacorrect($i, 1, INF); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// 888888
/// int(12)
/// imagegammacorrect(): Argument #2 ($input_gamma) must be greater than 0
/// imagegammacorrect(): Argument #3 ($output_gamma) must be finite
/// ```
#[php_function]
#[allow(clippy::many_single_char_names)]
fn imagegammacorrect(
    req: &mut Request,
    image: Instance<GdImage>,
    input_gamma: f64,
    output_gamma: f64,
) -> Result<True, Thrown> {
    for (position, value) in [(2, input_gamma), (3, output_gamma)] {
        if value <= 0.0 {
            return Err(arg_value_error(req, position, "be greater than 0"));
        }
        if !value.is_finite() {
            return Err(arg_value_error(req, position, "be finite"));
        }
    }
    let g = input_gamma / output_gamma;
    let im = im(req, image)?;
    if im.true_color() {
        for y in 0..im.sy() {
            for x in 0..im.sx() {
                let c = im.get_pixel(x, y);
                let [r, gr, b, a] = im.rgba(c);
                let next = (a << 24) | (gamma(r, g) << 16) | (gamma(gr, g) << 8) | gamma(b, g);
                im.set_pixel(x, y, next);
            }
        }
        return Ok(True);
    }
    for i in 0..im.colors_total() {
        let [r, gr, b, a] = im.rgba(i);
        im.set_palette(i, [gamma(r, g), gamma(gr, g), gamma(b, g), a]);
    }
    Ok(True)
}

/// `imagesetpixel(GdImage $image, int $x, int $y, int $color): true`.
///
/// ```php
/// $i = imagecreatetruecolor(1, 1);
/// var_dump(imagesetpixel($i, 0, 0, 0xFFEEDD), imagesetpixel($i, 5, 5, 1), dechex(imagecolorat($i, 0, 0)));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(6) "ffeedd"
/// ```
#[php_function]
fn imagesetpixel(
    req: &mut Request,
    image: Instance<GdImage>,
    x: i64,
    y: i64,
    color: i64,
) -> Result<True, Thrown> {
    let im = im(req, image)?;
    gd(req, || im.set_pixel(int(x), int(y), int(color)))?;
    Ok(True)
}

/// One of the box primitives, the colour turned to `gdAntiAliased`
/// where `aa` asks it and the image is anti-aliased.
fn draw(
    req: &mut Request,
    image: Instance<GdImage>,
    op: Draw,
    at: [i64; 4],
    color: i64,
    aa: bool,
) -> Result<True, Thrown> {
    let im = im(req, image)?;
    let mut c = int(color);
    if aa && im.aa() {
        im.set_anti_aliased(c);
        c = -7;
    }
    gd(req, || im.draw(op, at.map(int), c))?;
    Ok(True)
}

/// `imageline(GdImage $image, int $x1, int $y1, int $x2, int $y2, int
/// $color): true`: anti-aliased where `imageantialias` asked.
///
/// ```php
/// $i = imagecreatetruecolor(4, 4);
/// var_dump(imageline($i, 0, 0, 3, 3, 0xFF));
/// echo dechex(imagecolorat($i, 2, 2)), ' ', dechex(imagecolorat($i, 0, 3)), "\n";
/// imageantialias($i, true);
/// imageline($i, 0, 3, 3, 1, 0xFF0000);
/// echo dechex(imagecolorat($i, 1, 2)), "\n";
/// ```
/// ```expect
/// bool(true)
/// ff 0
/// aa0000
/// ```
#[php_function]
fn imageline(
    req: &mut Request,
    image: Instance<GdImage>,
    x1: i64,
    y1: i64,
    x2: i64,
    y2: i64,
    color: i64,
) -> Result<True, Thrown> {
    draw(req, image, Draw::Line, [x1, y1, x2, y2], color, true)
}

/// `imagedashedline(GdImage $image, int $x1, int $y1, int $x2, int $y2,
/// int $color): true`.
///
/// ```php
/// $i = imagecreatetruecolor(10, 1);
/// var_dump(imagedashedline($i, 0, 0, 9, 0, 0xFF));
/// for ($x = 0; $x < 10; $x++) echo imagecolorat($i, $x, 0) ? 1 : 0;
/// echo "\n";
/// ```
/// ```expect
/// bool(true)
/// 1110000111
/// ```
#[php_function]
fn imagedashedline(
    req: &mut Request,
    image: Instance<GdImage>,
    x1: i64,
    y1: i64,
    x2: i64,
    y2: i64,
    color: i64,
) -> Result<True, Thrown> {
    draw(req, image, Draw::Dashed, [x1, y1, x2, y2], color, false)
}

/// `imagerectangle(GdImage $image, int $x1, int $y1, int $x2, int $y2,
/// int $color): true`.
///
/// ```php
/// $i = imagecreatetruecolor(5, 5);
/// var_dump(imagerectangle($i, 0, 0, 4, 4, 0xFF));
/// echo imagecolorat($i, 0, 2), ' ', imagecolorat($i, 2, 2), "\n";
/// ```
/// ```expect
/// bool(true)
/// 255 0
/// ```
#[php_function]
fn imagerectangle(
    req: &mut Request,
    image: Instance<GdImage>,
    x1: i64,
    y1: i64,
    x2: i64,
    y2: i64,
    color: i64,
) -> Result<True, Thrown> {
    draw(req, image, Draw::Rectangle, [x1, y1, x2, y2], color, false)
}

/// `imagefilledrectangle(GdImage $image, int $x1, int $y1, int $x2, int
/// $y2, int $color): true`.
///
/// ```php
/// $i = imagecreatetruecolor(5, 5);
/// var_dump(imagefilledrectangle($i, 1, 1, 3, 3, 0xFF));
/// echo imagecolorat($i, 2, 2), ' ', imagecolorat($i, 4, 4), "\n";
/// ```
/// ```expect
/// bool(true)
/// 255 0
/// ```
#[php_function]
fn imagefilledrectangle(
    req: &mut Request,
    image: Instance<GdImage>,
    x1: i64,
    y1: i64,
    x2: i64,
    y2: i64,
    color: i64,
) -> Result<True, Thrown> {
    draw(req, image, Draw::Filled, [x1, y1, x2, y2], color, false)
}

/// `imagearc(GdImage $image, int $center_x, int $center_y, int $width,
/// int $height, int $start_angle, int $end_angle, int $color): true`.
///
/// ```php
/// $i = imagecreatetruecolor(9, 9);
/// var_dump(imagearc($i, 4, 4, 8, 8, 0, -90, 0xFF));
/// echo imagecolorat($i, 8, 4), ' ', imagecolorat($i, 4, 4), "\n";
/// ```
/// ```expect
/// bool(true)
/// 255 0
/// ```
#[php_function]
#[allow(clippy::too_many_arguments)]
fn imagearc(
    req: &mut Request,
    image: Instance<GdImage>,
    center_x: i64,
    center_y: i64,
    width: i64,
    height: i64,
    start_angle: i64,
    end_angle: i64,
    color: i64,
) -> Result<True, Thrown> {
    let im = im(req, image)?;
    let at = [center_x, center_y, width, height].map(int);
    let arc = [
        at[0],
        at[1],
        at[2],
        at[3],
        angle(start_angle),
        angle(end_angle),
    ];
    gd(req, || im.arc(arc, int(color), None))?;
    Ok(True)
}

/// `imageellipse(GdImage $image, int $center_x, int $center_y, int
/// $width, int $height, int $color): true`.
///
/// ```php
/// $i = imagecreatetruecolor(9, 9);
/// var_dump(imageellipse($i, 4, 4, 8, 8, 0xFF));
/// echo imagecolorat($i, 0, 4), ' ', imagecolorat($i, 4, 4), "\n";
/// ```
/// ```expect
/// bool(true)
/// 255 0
/// ```
#[php_function]
fn imageellipse(
    req: &mut Request,
    image: Instance<GdImage>,
    center_x: i64,
    center_y: i64,
    width: i64,
    height: i64,
    color: i64,
) -> Result<True, Thrown> {
    draw(
        req,
        image,
        Draw::Ellipse,
        [center_x, center_y, width, height],
        color,
        false,
    )
}

/// `imagefilltoborder(GdImage $image, int $x, int $y, int $border_color,
/// int $color): true`.
///
/// ```php
/// $i = imagecreatetruecolor(5, 5);
/// imagerectangle($i, 1, 1, 3, 3, 0xFF);
/// var_dump(imagefilltoborder($i, 2, 2, 0xFF, 0xFF00));
/// echo dechex(imagecolorat($i, 2, 2)), ' ', dechex(imagecolorat($i, 0, 0)), "\n";
/// ```
/// ```expect
/// bool(true)
/// ff00 0
/// ```
#[php_function]
fn imagefilltoborder(
    req: &mut Request,
    image: Instance<GdImage>,
    x: i64,
    y: i64,
    border_color: i64,
    color: i64,
) -> Result<True, Thrown> {
    draw(
        req,
        image,
        Draw::FillToBorder,
        [x, y, border_color, 0],
        color,
        false,
    )
}

/// `imagefill(GdImage $image, int $x, int $y, int $color): true`.
///
/// ```php
/// $i = imagecreatetruecolor(3, 3);
/// var_dump(imagefill($i, 0, 0, 0xFF00));
/// echo dechex(imagecolorat($i, 2, 2)), "\n";
/// ```
/// ```expect
/// bool(true)
/// ff00
/// ```
#[php_function]
fn imagefill(
    req: &mut Request,
    image: Instance<GdImage>,
    x: i64,
    y: i64,
    color: i64,
) -> Result<True, Thrown> {
    draw(req, image, Draw::Fill, [x, y, 0, 0], color, false)
}

/// `imagecolorstotal(GdImage $image): int`, zero for true color.
///
/// ```php
/// $p = imagecreate(1, 1); imagecolorallocate($p, 1, 1, 1); imagecolorallocate($p, 2, 2, 2);
/// var_dump(imagecolorstotal($p), imagecolorstotal(imagecreatetruecolor(1, 1)));
/// ```
/// ```expect
/// int(2)
/// int(0)
/// ```
#[php_function]
fn imagecolorstotal(req: &mut Request, image: Instance<GdImage>) -> Result<i64, Thrown> {
    Ok(i64::from(im(req, image)?.colors_total()))
}

/// `imagecolortransparent(GdImage $image, ?int $color = null): int`:
/// the transparent colour, set first where one is given.
///
/// ```php
/// $p = imagecreate(1, 1); imagecolorallocate($p, 1, 1, 1);
/// var_dump(imagecolortransparent($p), imagecolortransparent($p, 0), imagecolortransparent($p));
/// ```
/// ```expect
/// int(-1)
/// int(0)
/// int(0)
/// ```
#[php_function]
fn imagecolortransparent(
    req: &mut Request,
    image: Instance<GdImage>,
    #[php(default = null)] color: Option<i64>,
) -> Result<i64, Thrown> {
    let im = im(req, image)?;
    if let Some(color) = color {
        im.set_transparent(int(color));
    }
    Ok(i64::from(im.transparent()))
}

/// `imageinterlace(GdImage $image, ?bool $enable = null): bool`.
///
/// ```php
/// $i = imagecreatetruecolor(1, 1);
/// var_dump(imageinterlace($i), imageinterlace($i, true), imageinterlace($i));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn imageinterlace(
    req: &mut Request,
    image: Instance<GdImage>,
    #[php(default = null)] enable: Option<bool>,
) -> Result<bool, Thrown> {
    let im = im(req, image)?;
    if let Some(enable) = enable {
        im.interlace(enable);
    }
    Ok(im.interlaced())
}

/// `php_imagepolygon`: the points read as pairs from the array's integer
/// keys, `$num_points_or_color` the colour unless a fourth argument,
/// deprecated, makes it the count.
fn polygon(
    req: &mut Request,
    image: Instance<GdImage>,
    points: PhpArray,
    num_points_or_color: i64,
    color: Option<i64>,
    filled: i32,
) -> Result<bool, Thrown> {
    let elements = i64::try_from(points.len()).unwrap_or(i64::MAX);
    let (count, color) = match color {
        None => {
            if elements % 2 != 0 {
                return Err(arg_value_error(req, 2, "have an even number of elements"));
            }
            (elements / 2, num_points_or_color)
        }
        Some(color) => {
            docref(
                req,
                Level::Deprecated,
                "Using the $num_points parameter is deprecated",
            );
            (num_points_or_color, color)
        }
    };
    let im = im(req, image)?;
    let count = int(count);
    if count < 3 {
        return Err(arg_value_error(req, 3, "be greater than or equal to 3"));
    }
    if elements < i64::from(count) * 2 {
        let text = format!(
            "Trying to use {count} points in array with only {} points",
            elements / 2
        );
        return Err(value_error(req, &text));
    }
    let mut list = vec![Point::default(); usize::try_from(count).unwrap_or(0)];
    for (i, point) in (0_i64..).zip(list.iter_mut()) {
        if let Some(x) = points.get(&Key::Int(i * 2)) {
            point.x = int(to_long(req, &x)?);
        }
        if let Some(y) = points.get(&Key::Int(i * 2 + 1)) {
            point.y = int(to_long(req, &y)?);
        }
    }
    let mut c = int(color);
    if im.aa() {
        im.set_anti_aliased(c);
        c = -7;
    }
    gd(req, || im.polygon(&mut list, c, filled))?;
    Ok(true)
}

/// `imagepolygon(GdImage $image, array $points, int
/// $num_points_or_color, ?int $color = null): bool`.
///
/// ```php
/// $i = imagecreatetruecolor(5, 5);
/// var_dump(imagepolygon($i, [0, 0, 4, 0, 4, 4], 0xFF));
/// echo imagecolorat($i, 2, 2), ' ', imagecolorat($i, 1, 2), "\n";
/// var_dump(imagepolygon($i, [0, 0, 4, 0, 4, 4], 3, 0xFF));
/// try { imagepolygon($i, [0, 0, 1], 0xFF); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imagepolygon($i, [0, 0, 1, 1], 0xFF); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imagepolygon($i, [0, 0, 1, 1], 3, 0xFF); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// 255 0
///
/// Deprecated: imagepolygon(): Using the $num_points parameter is deprecated in %s on line 5
/// bool(true)
/// imagepolygon(): Argument #2 ($points) must have an even number of elements
/// imagepolygon(): Argument #3 ($num_points_or_color) must be greater than or equal to 3
///
/// Deprecated: imagepolygon(): Using the $num_points parameter is deprecated in %s on line 8
/// Trying to use 3 points in array with only 2 points
/// ```
#[php_function]
fn imagepolygon(
    req: &mut Request,
    image: Instance<GdImage>,
    points: PhpArray,
    num_points_or_color: i64,
    #[php(default = null)] color: Option<i64>,
) -> Result<bool, Thrown> {
    polygon(req, image, points, num_points_or_color, color, 0)
}

/// `imageopenpolygon(GdImage $image, array $points, int
/// $num_points_or_color, ?int $color = null): bool`.
///
/// ```php
/// $i = imagecreatetruecolor(5, 5);
/// var_dump(imageopenpolygon($i, [0, 0, 4, 0, 4, 4], 0xFF));
/// echo imagecolorat($i, 2, 2), ' ', imagecolorat($i, 4, 2), "\n";
/// ```
/// ```expect
/// bool(true)
/// 0 255
/// ```
#[php_function]
fn imageopenpolygon(
    req: &mut Request,
    image: Instance<GdImage>,
    points: PhpArray,
    num_points_or_color: i64,
    #[php(default = null)] color: Option<i64>,
) -> Result<bool, Thrown> {
    polygon(req, image, points, num_points_or_color, color, -1)
}

/// `imagefilledpolygon(GdImage $image, array $points, int
/// $num_points_or_color, ?int $color = null): bool`.
///
/// ```php
/// $i = imagecreatetruecolor(5, 5);
/// var_dump(imagefilledpolygon($i, [0, 0, 4, 0, 4, 4], 0xFF));
/// echo imagecolorat($i, 3, 1), ' ', imagecolorat($i, 0, 4), "\n";
/// ```
/// ```expect
/// bool(true)
/// 255 0
/// ```
#[php_function]
fn imagefilledpolygon(
    req: &mut Request,
    image: Instance<GdImage>,
    points: PhpArray,
    num_points_or_color: i64,
    #[php(default = null)] color: Option<i64>,
) -> Result<bool, Thrown> {
    polygon(req, image, points, num_points_or_color, color, 1)
}

/// `imagefontwidth(GdFont|int $font): int`.
///
/// ```php
/// foreach ([0, 1, 2, 3, 4, 5, 6] as $f) echo imagefontwidth($f), ' ';
/// echo "\n";
/// ```
/// ```expect
/// 5 5 6 7 8 9 9
/// ```
#[php_function]
fn imagefontwidth(req: &mut Request, font: Union<Instance<GdFont>, i64>) -> Result<i64, Thrown> {
    with_font(req, font, |_, f| Ok(i64::from(f.size().0)))
}

/// `imagefontheight(GdFont|int $font): int`.
///
/// ```php
/// foreach ([0, 1, 2, 3, 4, 5, 6] as $f) echo imagefontheight($f), ' ';
/// echo "\n";
/// ```
/// ```expect
/// 8 8 13 13 16 15 15
/// ```
#[php_function]
fn imagefontheight(req: &mut Request, font: Union<Instance<GdFont>, i64>) -> Result<i64, Thrown> {
    with_font(req, font, |_, f| Ok(i64::from(f.size().1)))
}

/// `php_gdimagecharup`, php's own rotated glyph: column by column from
/// `(x, y)` upwards.
#[allow(clippy::many_single_char_names)]
fn char_up(im: crate::ffi::Im, font: &Font, [x, y]: [i32; 2], c: i32, color: i32) {
    let (nchars, offset, data) = font.glyphs();
    let (w, h) = font.size();
    if c < offset || c >= offset.wrapping_add(nchars) {
        return;
    }
    let line = usize::try_from((c - offset) * h * w).unwrap_or(0);
    let mut cx = 0;
    let mut py = y;
    while py > y - w {
        let mut cy = 0;
        let mut px = x;
        while px < x + h {
            let at = line + usize::try_from(cy * w + cx).unwrap_or(0);
            if data.get(at).is_some_and(|b| *b != 0) {
                im.set_pixel(px, py, color);
            }
            cy += 1;
            px += 1;
        }
        cx += 1;
        py -= 1;
    }
}

/// `php_imagechar`: kind 0 `imagechar`, 1 `imagecharup`, 2
/// `imagestring`, 3 `imagestringup`.
#[allow(clippy::too_many_arguments)]
fn text(
    req: &mut Request,
    image: Instance<GdImage>,
    font: Union<Instance<GdFont>, i64>,
    x: i64,
    y: i64,
    string: PhpString,
    color: i64,
    mode: u8,
) -> Result<True, Thrown> {
    let im = im(req, image)?;
    let col = int(color);
    let bytes = string.as_bytes();
    let bytes = php_ext::coerce::c_str(bytes);
    let (mut x, mut y) = (int(x), int(y));
    let first = i32::from(string.as_bytes().first().copied().unwrap_or(0));
    with_font(req, font, |req, f| {
        gd(req, || match mode {
            0 => im.char(f, false, [x, y], first, col),
            1 => char_up(im, f, [x, y], first, col),
            2 => {
                for b in bytes {
                    im.char(f, false, [x, y], i32::from(*b), col);
                    x = x.wrapping_add(f.size().0);
                }
            }
            _ => {
                for b in bytes {
                    im.char(f, true, [x, y], i32::from(*b), col);
                    y = y.wrapping_sub(f.size().0);
                }
            }
        })?;
        Ok(True)
    })
}

/// `imagechar(GdImage $image, GdFont|int $font, int $x, int $y, string
/// $char, int $color): true`: the first byte of `$char`.
///
/// ```php
/// $i = imagecreatetruecolor(8, 8);
/// var_dump(imagechar($i, 1, 0, 0, "Hello", 0xFF));
/// $n = 0; for ($y = 0; $y < 8; $y++) for ($x = 0; $x < 8; $x++) $n += imagecolorat($i, $x, $y) ? 1 : 0;
/// var_dump($n);
/// ```
/// ```expect
/// bool(true)
/// int(14)
/// ```
#[php_function]
fn imagechar(
    req: &mut Request,
    image: Instance<GdImage>,
    font: Union<Instance<GdFont>, i64>,
    x: i64,
    y: i64,
    char: PhpString,
    color: i64,
) -> Result<True, Thrown> {
    text(req, image, font, x, y, char, color, 0)
}

/// `imagecharup(GdImage $image, GdFont|int $font, int $x, int $y, string
/// $char, int $color): true`.
///
/// ```php
/// $i = imagecreatetruecolor(8, 8);
/// var_dump(imagecharup($i, 1, 0, 7, "H", 0xFF));
/// $n = 0; for ($y = 0; $y < 8; $y++) for ($x = 0; $x < 8; $x++) $n += imagecolorat($i, $x, $y) ? 1 : 0;
/// var_dump($n);
/// ```
/// ```expect
/// bool(true)
/// int(14)
/// ```
#[php_function]
fn imagecharup(
    req: &mut Request,
    image: Instance<GdImage>,
    font: Union<Instance<GdFont>, i64>,
    x: i64,
    y: i64,
    char: PhpString,
    color: i64,
) -> Result<True, Thrown> {
    text(req, image, font, x, y, char, color, 1)
}

/// `imagestring(GdImage $image, GdFont|int $font, int $x, int $y, string
/// $string, int $color): true`: to the first NUL, as `strlen` reads it.
///
/// ```php
/// $i = imagecreatetruecolor(40, 10);
/// var_dump(imagestring($i, 2, 0, 0, "Hi\0there", 0xFF));
/// $n = 0; for ($y = 0; $y < 10; $y++) for ($x = 0; $x < 40; $x++) $n += imagecolorat($i, $x, $y) ? 1 : 0;
/// var_dump($n);
/// ```
/// ```expect
/// bool(true)
/// int(24)
/// ```
#[php_function]
fn imagestring(
    req: &mut Request,
    image: Instance<GdImage>,
    font: Union<Instance<GdFont>, i64>,
    x: i64,
    y: i64,
    string: PhpString,
    color: i64,
) -> Result<True, Thrown> {
    text(req, image, font, x, y, string, color, 2)
}

/// `imagestringup(GdImage $image, GdFont|int $font, int $x, int $y,
/// string $string, int $color): true`.
///
/// ```php
/// $i = imagecreatetruecolor(10, 40);
/// var_dump(imagestringup($i, 2, 0, 39, "Hi", 0xFF));
/// $n = 0; for ($y = 0; $y < 40; $y++) for ($x = 0; $x < 10; $x++) $n += imagecolorat($i, $x, $y) ? 1 : 0;
/// var_dump($n);
/// ```
/// ```expect
/// bool(true)
/// int(24)
/// ```
#[php_function]
fn imagestringup(
    req: &mut Request,
    image: Instance<GdImage>,
    font: Union<Instance<GdFont>, i64>,
    x: i64,
    y: i64,
    string: PhpString,
    color: i64,
) -> Result<True, Thrown> {
    text(req, image, font, x, y, string, color, 3)
}

/// `imagecopy(GdImage $dst_image, GdImage $src_image, int $dst_x, int
/// $dst_y, int $src_x, int $src_y, int $src_width, int $src_height):
/// true`.
///
/// ```php
/// $s = imagecreatetruecolor(2, 2); imagefill($s, 0, 0, 0xFF);
/// $d = imagecreatetruecolor(4, 4);
/// var_dump(imagecopy($d, $s, 2, 2, 0, 0, 2, 2));
/// echo imagecolorat($d, 3, 3), ' ', imagecolorat($d, 1, 1), "\n";
/// ```
/// ```expect
/// bool(true)
/// 255 0
/// ```
#[php_function]
#[allow(clippy::too_many_arguments)]
fn imagecopy(
    req: &mut Request,
    dst_image: Instance<GdImage>,
    src_image: Instance<GdImage>,
    dst_x: i64,
    dst_y: i64,
    src_x: i64,
    src_y: i64,
    src_width: i64,
    src_height: i64,
) -> Result<True, Thrown> {
    let at = [dst_x, dst_y, src_x, src_y];
    copy(
        req,
        dst_image,
        src_image,
        Copy::Plain,
        at,
        [0, 0, src_width, src_height],
    )
}

/// `imagecopymerge(GdImage $dst_image, GdImage $src_image, int $dst_x,
/// int $dst_y, int $src_x, int $src_y, int $src_width, int $src_height,
/// int $pct): true`.
///
/// ```php
/// $s = imagecreatetruecolor(1, 1); imagefill($s, 0, 0, 0xFFFFFF);
/// $d = imagecreatetruecolor(1, 1);
/// var_dump(imagecopymerge($d, $s, 0, 0, 0, 0, 1, 1, 50));
/// echo dechex(imagecolorat($d, 0, 0)), "\n";
/// ```
/// ```expect
/// bool(true)
/// 7f7f7f
/// ```
#[php_function]
#[allow(clippy::too_many_arguments)]
fn imagecopymerge(
    req: &mut Request,
    dst_image: Instance<GdImage>,
    src_image: Instance<GdImage>,
    dst_x: i64,
    dst_y: i64,
    src_x: i64,
    src_y: i64,
    src_width: i64,
    src_height: i64,
    pct: i64,
) -> Result<True, Thrown> {
    let at = [dst_x, dst_y, src_x, src_y];
    copy(
        req,
        dst_image,
        src_image,
        Copy::Merge,
        at,
        [pct, 0, src_width, src_height],
    )
}

/// `imagecopymergegray(GdImage $dst_image, GdImage $src_image, int
/// $dst_x, int $dst_y, int $src_x, int $src_y, int $src_width, int
/// $src_height, int $pct): true`.
///
/// ```php
/// $s = imagecreatetruecolor(1, 1); imagefill($s, 0, 0, 0xFF0000);
/// $d = imagecreatetruecolor(1, 1); imagefill($d, 0, 0, 0x00FF00);
/// var_dump(imagecopymergegray($d, $s, 0, 0, 0, 0, 1, 1, 50));
/// echo dechex(imagecolorat($d, 0, 0)), "\n";
/// ```
/// ```expect
/// bool(true)
/// ca4a4a
/// ```
#[php_function]
#[allow(clippy::too_many_arguments)]
fn imagecopymergegray(
    req: &mut Request,
    dst_image: Instance<GdImage>,
    src_image: Instance<GdImage>,
    dst_x: i64,
    dst_y: i64,
    src_x: i64,
    src_y: i64,
    src_width: i64,
    src_height: i64,
    pct: i64,
) -> Result<True, Thrown> {
    let at = [dst_x, dst_y, src_x, src_y];
    copy(
        req,
        dst_image,
        src_image,
        Copy::MergeGray,
        at,
        [pct, 0, src_width, src_height],
    )
}

/// `imagecopyresized(GdImage $dst_image, GdImage $src_image, int $dst_x,
/// int $dst_y, int $src_x, int $src_y, int $dst_width, int $dst_height,
/// int $src_width, int $src_height): true`: each size above zero.
///
/// ```php
/// $s = imagecreatetruecolor(1, 1); imagefill($s, 0, 0, 0xFF);
/// $d = imagecreatetruecolor(3, 3);
/// var_dump(imagecopyresized($d, $s, 0, 0, 0, 0, 3, 3, 1, 1));
/// echo imagecolorat($d, 2, 2), "\n";
/// try { imagecopyresized($d, $s, 0, 0, 0, 0, 3, 3, 1, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// 255
/// imagecopyresized(): Argument #10 ($src_height) must be greater than 0
/// ```
#[php_function]
#[allow(clippy::too_many_arguments)]
fn imagecopyresized(
    req: &mut Request,
    dst_image: Instance<GdImage>,
    src_image: Instance<GdImage>,
    dst_x: i64,
    dst_y: i64,
    src_x: i64,
    src_y: i64,
    dst_width: i64,
    dst_height: i64,
    src_width: i64,
    src_height: i64,
) -> Result<True, Thrown> {
    let size = [dst_width, dst_height, src_width, src_height];
    let (d, s) = (im(req, dst_image)?, im(req, src_image)?);
    for (position, n) in (7..).zip(size) {
        if int(n) <= 0 {
            return Err(arg_value_error(req, position, "be greater than 0"));
        }
    }
    let at = [dst_x, dst_y, src_x, src_y].map(int);
    gd(req, || d.copy(s, Copy::Resized, at, size.map(int)))?;
    Ok(True)
}

/// `imagesx(GdImage $image): int`.
///
/// ```php
/// var_dump(imagesx(imagecreate(7, 3)));
/// ```
/// ```expect
/// int(7)
/// ```
#[php_function]
fn imagesx(req: &mut Request, image: Instance<GdImage>) -> Result<i64, Thrown> {
    Ok(i64::from(im(req, image)?.sx()))
}

/// `imagesy(GdImage $image): int`.
///
/// ```php
/// var_dump(imagesy(imagecreate(7, 3)));
/// ```
/// ```expect
/// int(3)
/// ```
#[php_function]
fn imagesy(req: &mut Request, image: Instance<GdImage>) -> Result<i64, Thrown> {
    Ok(i64::from(im(req, image)?.sy()))
}

/// `imagesetclip(GdImage $image, int $x1, int $y1, int $x2, int $y2):
/// true`: libgd clamps the rectangle to the image.
///
/// ```php
/// $i = imagecreatetruecolor(10, 10);
/// var_dump(imagesetclip($i, 2, 3, 40, -1), imagegetclip($i));
/// ```
/// ```expect
/// bool(true)
/// array(4) {
///   [0]=>
///   int(2)
///   [1]=>
///   int(3)
///   [2]=>
///   int(9)
///   [3]=>
///   int(0)
/// }
/// ```
#[php_function]
fn imagesetclip(
    req: &mut Request,
    image: Instance<GdImage>,
    x1: i64,
    y1: i64,
    x2: i64,
    y2: i64,
) -> Result<True, Thrown> {
    draw(req, image, Draw::Clip, [x1, y1, x2, y2], 0, false)
}

/// `imagegetclip(GdImage $image): array`.
///
/// ```php
/// var_dump(imagegetclip(imagecreatetruecolor(3, 2)));
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   int(0)
///   [1]=>
///   int(0)
///   [2]=>
///   int(2)
///   [3]=>
///   int(1)
/// }
/// ```
#[php_function]
fn imagegetclip(req: &mut Request, image: Instance<GdImage>) -> Result<Owner<PhpArray>, Thrown> {
    let clip = im(req, image)?.clip().map(|n| Value::int(i64::from(n)));
    crate::list(req, &clip)
}
