//! The `exif` extension: `php-src/ext/exif` as one crate under `ext/`
//! (ADR-0004), `exif.c` ported with no library (design §7.2). One
//! dependency beside the facade, `standard`, whose `php_image.h` and
//! `php_string.h` exif includes; one entry, `extension()`; no state
//! (ADR-0005): the six `exif.*` ini entries are read as a file is. The
//! phpt subset is `php-src/ext/exif/tests`, through `xtask phpt
//! --subset`.
//!
//! [`walk`] is the reader, [`image`] what it collects, [`tags`] the
//! tag names and maker notes, [`decode`] the multibyte calls, and this
//! file the four natives over them.
#![forbid(unsafe_code)]
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

mod decode;
mod image;
mod tags;
mod walk;

use php_ext::abi::{
    Dependency, DependencyKind, Extension, ExtensionBuilder, OrFalse, Path, Ref, Untyped,
};
use php_ext::attr::{php_const, php_function};
use php_ext::coerce::c_str;
use php_ext::coerce::to_string;
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw_error};
use php_ext::request::{Opening, OwnedStream as _, Request, Stream, Streams as _, Whence};
use php_ext::values::{Owner, PhpArray, PhpString, Value, ValueRef};

use image::{EncodingNames, Found, Image, Number, Section, Stored, TAG_NONE, c_int};
use walk::Reader;

/// The ini entries, `PHP_INI_BEGIN()`'s order.
const INI: [(&[u8], &[u8]); 6] = [
    (b"exif.encode_unicode", b"ISO-8859-15"),
    (b"exif.decode_unicode_motorola", b"UCS-2BE"),
    (b"exif.decode_unicode_intel", b"UCS-2LE"),
    (b"exif.encode_jis", b""),
    (b"exif.decode_jis_motorola", b"JIS"),
    (b"exif.decode_jis_intel", b"JIS"),
];

/// `EXIF_USE_MBSTRING`, which the bundle's `exif` feature makes true by
/// turning mbstring on beside it.
///
/// ```php
/// var_dump(EXIF_USE_MBSTRING);
/// ```
/// ```expect
/// int(1)
/// ```
#[php_const]
pub(crate) mod constants {
    pub(crate) const EXIF_USE_MBSTRING: i64 = 1;
}

/// The extension value (design §7.1): `exif_module_entry`, its
/// dependencies, ini entries, constant and four functions.
///
/// ```php
/// var_dump(extension_loaded('exif'), get_extension_funcs('exif'), ini_get('exif.encode_unicode'), ini_get('exif.decode_jis_intel'));
/// ```
/// ```expect
/// bool(true)
/// array(4) {
///   [0]=>
///   string(12) "exif_tagname"
///   [1]=>
///   string(14) "exif_read_data"
///   [2]=>
///   string(14) "exif_thumbnail"
///   [3]=>
///   string(14) "exif_imagetype"
/// }
/// string(11) "ISO-8859-15"
/// string(3) "JIS"
/// ```
pub fn extension() -> Extension {
    let dependency = |name: &str, kind| Dependency {
        name: String::from(name),
        kind,
        rel: None,
        version: None,
    };
    let mut builder = ExtensionBuilder::default()
        .dependencies(vec![
            dependency("standard", DependencyKind::Required),
            dependency("mbstring", DependencyKind::Optional),
        ])
        .constants(constants::declared())
        .functions(vec![
            exif_tagname::declared(),
            exif_read_data::declared(),
            exif_thumbnail::declared(),
            exif_imagetype::declared(),
        ])
        .ini(
            INI.iter()
                .map(|(name, default)| IniDecl {
                    name: Name((*name).into()),
                    default: Some((*default).into()),
                    user: true,
                    per_dir: true,
                    system: true,
                })
                .collect(),
        );
    for (name, _) in INI {
        let on_modify = if name.starts_with(b"exif.encode") {
            on_update_encode
        } else {
            on_update_decode
        };
        builder = builder.on_modify(name, on_modify);
    }
    builder.info(exif_info).build("exif")
}

/// `PHP_MINFO_FUNCTION(exif)`.
///
/// ```php
/// (new ReflectionExtension('exif'))->info();
/// ```
/// ```expect
///
/// exif
///
/// EXIF Support => enabled
/// Supported EXIF Version => 0220
/// Supported filetypes => JPEG, TIFF
/// Multibyte decoding support using mbstring => enabled
/// Extended EXIF tag formats => Canon, Casio, Fujifilm, Nikon, Olympus, Samsung, Panasonic, DJI, Sony, Pentax, Minolta, Sigma, Foveon, Kyocera, Ricoh, AGFA, Epson
///
/// Directive => Local Value => Master Value
/// exif.encode_unicode => ISO-8859-15 => ISO-8859-15
/// exif.decode_unicode_motorola => UCS-2BE => UCS-2BE
/// exif.decode_unicode_intel => UCS-2LE => UCS-2LE
/// exif.encode_jis => no value => no value
/// exif.decode_jis_motorola => JIS => JIS
/// exif.decode_jis_intel => JIS => JIS
/// ```
fn exif_info(req: &mut Request) {
    let mbstring = if decode::provided(req) {
        "enabled"
    } else {
        "disabled"
    };
    req.info_table(&[
        &["EXIF Support", "enabled"],
        &["Supported EXIF Version", "0220"],
        &["Supported filetypes", "JPEG, TIFF"],
        &["Multibyte decoding support using mbstring", mbstring],
        &[
            "Extended EXIF tag formats",
            "Canon, Casio, Fujifilm, Nikon, Olympus, Samsung, Panasonic, DJI, Sony, Pentax, Minolta, Sigma, Foveon, Kyocera, Ricoh, AGFA, Epson",
        ],
    ]);
    let names: Vec<&[u8]> = INI.iter().map(|(name, _)| *name).collect();
    req.info_ini_entries(&names);
}

/// `OnUpdateEncode`: a non-empty value must parse as an encoding list.
#[expect(clippy::unnecessary_wraps, reason = "the `on_modify` slot's shape")]
fn on_update_encode(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    Ok(value.is_none_or(|value| value.is_empty() || parses(req, value)))
}

/// `OnUpdateDecode`: the value must parse as an encoding list.
#[expect(clippy::unnecessary_wraps, reason = "the `on_modify` slot's shape")]
fn on_update_decode(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    Ok(value.is_none_or(|value| parses(req, value)))
}

/// `zend_multibyte_parse_encoding_list`, with php's warning where it
/// fails.
fn parses(req: &mut Request, value: &[u8]) -> bool {
    if decode::parses(req, value) {
        return true;
    }
    let value = String::from_utf8_lossy(c_str(value)).into_owned();
    docref(
        req,
        Level::Warning,
        &format!("Illegal encoding ignored: '{value}'"),
    );
    false
}

/// The `exif.*` entries as the request holds them.
fn encodings(req: &Request) -> EncodingNames {
    let get = |at: usize| {
        req.ini_get(INI[at].0)
            .map(<[u8]>::to_vec)
            .unwrap_or_default()
    };
    EncodingNames {
        encode_unicode: get(0),
        decode_unicode_be: get(1),
        decode_unicode_le: get(2),
        encode_jis: get(3),
        decode_jis_be: get(4),
        decode_jis_le: get(5),
    }
}

/// `exif_tagname(int $index): string|false`: the IFD name of a tag.
///
/// ```php
/// var_dump(exif_tagname(0x10F), exif_tagname(0x9286), exif_tagname(0xFFFF), exif_tagname(0x1234), exif_tagname(-1));
/// ```
/// ```expect
/// string(4) "Make"
/// string(11) "UserComment"
/// string(12) "No tag value"
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn exif_tagname(req: &mut Request, index: i64) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    // php looks the `int` the long truncates to up in a table keyed by
    // the `unsigned short` tags.
    #[expect(clippy::cast_possible_truncation, reason = "php's `int tag_num`")]
    let tag = u16::try_from(index as i32).ok();
    match tag
        .filter(|_| index >= 0)
        .and_then(|tag| tags::name(tag, tags::IFD))
    {
        Some(name) => PhpString::new(req, name.as_bytes()).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `exif_read_data($file, ?string $required_sections = null, bool
/// $as_arrays = false, bool $read_thumbnail = false): array|false`: a
/// JPEG, TIFF or HEIF file's headers as php's sections.
///
/// ```php
/// $tiff = "MM\0\x2A\0\0\0\x08\0\x03" . "\x01\x0F\0\x02\0\0\0\x04ACM\0" . "\x01\x12\0\x03\0\0\0\x01\0\x06\0\0" . "\x82\x98\0\x02\0\0\0\x04A\0B\0" . "\0\0\0\0";
/// $f = fopen('php://memory', 'w+');
/// fwrite($f, $tiff);
/// var_dump(exif_read_data($f));
/// var_dump(array_keys(exif_read_data($f, 'IFD0', true)), exif_read_data($f, 'GPS'));
/// ```
/// ```expect
/// array(9) {
///   ["FileDateTime"]=>
///   int(0)
///   ["FileSize"]=>
///   int(50)
///   ["FileType"]=>
///   int(8)
///   ["MimeType"]=>
///   string(10) "image/tiff"
///   ["SectionsFound"]=>
///   string(13) "ANY_TAG, IFD0"
///   ["COMPUTED"]=>
///   array(5) {
///     ["IsColor"]=>
///     int(0)
///     ["ByteOrderMotorola"]=>
///     int(1)
///     ["Copyright"]=>
///     string(4) "A, B"
///     ["Copyright.Photographer"]=>
///     string(1) "A"
///     ["Copyright.Editor"]=>
///     string(1) "B"
///   }
///   ["Make"]=>
///   string(3) "ACM"
///   ["Orientation"]=>
///   int(6)
///   ["Copyright"]=>
///   string(1) "A"
/// }
/// array(3) {
///   [0]=>
///   string(4) "FILE"
///   [1]=>
///   string(8) "COMPUTED"
///   [2]=>
///   string(4) "IFD0"
/// }
/// bool(false)
/// ```
///
/// A path that does not open, and a directory entry past the file:
///
/// ```php
/// var_dump(exif_read_data('/nonexistent/x.jpg'));
/// $f = fopen('php://memory', 'w+');
/// fwrite($f, "II\x2A\0\x08\0\0\0\x01\0" . "\x0F\x01\x02\0\x40\0\0\0\xFF\0\0\0" . "\0\0\0\0");
/// var_dump(exif_read_data($f));
/// ```
/// ```expect
/// Warning: exif_read_data(): Unable to open file in %s on line 2
/// bool(false)
///
/// Warning: exif_read_data(): Process tag(x010F=Make): Illegal pointer offset(x00FF + x0040 = x013F > x001A) in %s on line 5
///
/// Warning: exif_read_data(): Invalid TIFF file in %s on line 5
/// bool(false)
/// ```
#[php_function]
fn exif_read_data(
    req: &mut Request,
    file: Untyped<Value>,
    #[php(default = null)] required_sections: Option<PhpString>,
    #[php(default = false)] as_arrays: bool,
    #[php(default = false)] read_thumbnail: bool,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Untyped(file) = file;
    let needed = required_sections.map(|names| walk::needed(names.as_bytes()));
    let (mut reader, ok) = read(req, &file, read_thumbnail)?;
    let found = reader.img.found;
    reader.img.found.add(Section::Computed);
    reader.img.found.add(Section::File);
    let missing = needed.is_some_and(|n| n != Found::default() && !n.meets(reader.img.found));
    if !ok || missing {
        return Ok(OrFalse::False);
    }
    computed(&mut reader, &found.list());
    let mut array = PhpArray::new(reader.req)?;
    for section in OUTPUT {
        let sub = as_arrays
            || matches!(
                section,
                Section::Computed | Section::Thumbnail | Section::Comment
            );
        add_section(reader.req, &mut array, &reader.img, section, sub)?;
    }
    Ok(OrFalse::Value(array))
}

/// The sections `exif_read_data` returns, in its order: `APP0` never,
/// and `GPS` and `INTEROP` before `FPIX`.
const OUTPUT: [Section; 13] = [
    Section::File,
    Section::Computed,
    Section::AnyTag,
    Section::Ifd0,
    Section::Thumbnail,
    Section::Comment,
    Section::Exif,
    Section::Gps,
    Section::Interop,
    Section::Fpix,
    Section::App12,
    Section::WinXp,
    Section::MakerNote,
];

/// What `exif_read_data` adds after the read: the `FILE` section, the
/// `COMPUTED` one, the Windows XP fields and the thumbnail's.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "php's `int` and `%d` of its `time_t`, `size_t` and `float` fields"
)]
fn computed(reader: &mut Reader<'_>, sections: &str) {
    use Section::{Computed, File, Thumbnail, WinXp};
    let img = &mut reader.img;
    let name = img.file_name.clone();
    img.add_str(File, b"FileName", name.as_deref());
    img.add_int(File, "FileDateTime", img.file_date_time as i32);
    img.add_int(File, "FileSize", c_int(img.file_size));
    img.add_int(File, "FileType", img.file_type.code() as i32);
    let mime = php_ext_standard::php_image_type_to_mime_type(reader.req, img.file_type.code());
    img.add_str(File, b"MimeType", Some(mime.as_bytes()));
    img.add_str(File, b"SectionsFound", Some(sections.as_bytes()));
    if img.width > 0 && img.height > 0 {
        let html = format!("width=\"{}\" height=\"{}\"", img.width, img.height);
        img.add_str(Computed, b"html", Some(html.as_bytes()));
        img.add_int(Computed, "Height", img.height);
        img.add_int(Computed, "Width", img.width);
    }
    img.add_int(Computed, "IsColor", i32::from(img.is_color));
    if let Some(order) = img.order {
        img.add_int(
            Computed,
            "ByteOrderMotorola",
            i32::from(order == image::Order::Motorola),
        );
    }
    if img.ccd_width != 0.0 {
        let text = format!("{}mm", img.ccd_width as i32);
        img.add_str(Computed, b"CCDWidth", Some(text.as_bytes()));
    }
    if img.exposure_time > 0.0 {
        let recip = 0.5f32 + 1.0 / img.exposure_time;
        let text = if f64::from(img.exposure_time) <= 0.5 && recip < i32::MAX as f32 {
            format!("{:.3} s (1/{})", img.exposure_time, recip as i32)
        } else {
            format!("{:.3} s", img.exposure_time)
        };
        img.add_str(Computed, b"ExposureTime", Some(text.as_bytes()));
    }
    if img.aperture_f_number != 0.0 {
        let text = format!("f/{:.1}", img.aperture_f_number);
        img.add_str(Computed, b"ApertureFNumber", Some(text.as_bytes()));
    }
    if img.distance != 0.0 {
        let text = if img.distance < 0.0 {
            String::from("Infinite")
        } else {
            format!("{:.2}m", img.distance)
        };
        img.add_str(Computed, b"FocusDistance", Some(text.as_bytes()));
    }
    if let Some(comment) = img.user_comment.clone() {
        img.add_buffer(Computed, "UserComment", &comment);
        if let Some(encoding) = img.user_comment_encoding.clone().filter(|e| !e.is_empty()) {
            img.add_str(Computed, b"UserCommentEncoding", Some(&encoding));
        }
    }
    let copyright = [
        (&b"Copyright"[..], img.copyright.clone()),
        (
            b"Copyright.Photographer",
            img.copyright_photographer.clone(),
        ),
        (b"Copyright.Editor", img.copyright_editor.clone()),
    ];
    for (name, value) in copyright {
        img.add_str(Computed, name, value.as_deref());
    }
    for (tag, value) in img.xp_fields.clone() {
        let name = tags::debug_name(tag, WinXp.table());
        img.add_str(WinXp, name.as_bytes(), value.as_deref());
    }
    if img.thumbnail.size != 0 {
        if img.read_thumbnail {
            let data = img.thumbnail.data.clone();
            let size = usize::try_from(img.thumbnail.size).unwrap_or(0);
            let format = image::Format::Undefined;
            let name = b"THUMBNAIL".to_vec();
            let warning = img.add_value(Thumbnail, name, TAG_NONE, format, size, data.as_deref());
            if let Some(warning) = warning {
                reader.error(Level::Warning, &warning);
            }
        }
        let thumb = &reader.img.thumbnail;
        if thumb.width == 0 || thumb.height == 0 {
            reader.scan_thumbnail();
        }
        let filetype = reader.img.thumbnail.filetype.code();
        reader
            .img
            .add_int(Computed, "Thumbnail.FileType", filetype as i32);
        let mime = php_ext_standard::php_image_type_to_mime_type(reader.req, filetype);
        reader
            .img
            .add_str(Computed, b"Thumbnail.MimeType", Some(mime.as_bytes()));
    }
    let img = &mut reader.img;
    if img.thumbnail.width != 0 && img.thumbnail.height != 0 {
        img.add_int(Computed, "Thumbnail.Height", c_int(img.thumbnail.height));
        img.add_int(Computed, "Thumbnail.Width", c_int(img.thumbnail.width));
    }
}

/// `add_assoc_image_info`: one section's values into `array`, or into
/// an array of their own under the section's name.
fn add_section(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    img: &Image,
    section: Section,
    sub_array: bool,
) -> Result<(), Thrown> {
    let entries = &img.sections[section.index()];
    if entries.is_empty() {
        return Ok(());
    }
    let mut own = if sub_array {
        Some(PhpArray::new(req)?)
    } else {
        None
    };
    let target = match own.as_mut() {
        Some(own) => own,
        None => &mut *array,
    };
    let mut index = 0;
    for entry in entries {
        let value = if entry.length == 0 {
            Owner::null()
        } else {
            match &entry.value {
                Stored::Text(text) if section == Section::Comment => {
                    let text = PhpString::new(req, text)?.into_value();
                    target.set(req, Owner::index(index), text)?;
                    index += 1;
                    continue;
                }
                Stored::Bytes(bytes) | Stored::Text(bytes) => {
                    PhpString::new(req, bytes)?.into_value()
                }
                Stored::Numbers(numbers) => match numbers.as_slice() {
                    [one] => number(req, *one)?,
                    many => {
                        let mut list = PhpArray::new(req)?;
                        for (at, one) in (0..).zip(many) {
                            let value = number(req, *one)?;
                            list.set(req, Owner::index(at), value)?;
                        }
                        list.into_value()
                    }
                },
            }
        };
        target.set_symtable_str(req, &entry.name, value)?;
    }
    if let Some(own) = own {
        array.set_str(req, section.name().as_bytes(), own.into_value())?;
    }
    Ok(())
}

/// One number as `add_assoc_image_info` renders it: an `int` of the
/// `int` php casts to, a rational as its `%u/%u` or `%i/%i` string, a
/// float as a double.
fn number(req: &mut Request, number: Number) -> Result<Owner<Value>, Thrown> {
    let text = |req: &mut Request, text: String| {
        PhpString::new(req, text.as_bytes()).map(Owner::into_value)
    };
    let scalar = match number {
        Number::Unsigned(u) => Value::int(i64::from(i32::from_ne_bytes(u.to_ne_bytes()))),
        Number::Signed(i) => Value::int(i64::from(i)),
        Number::URational(n, d) => return text(req, format!("{n}/{d}")),
        Number::SRational(n, d) => return text(req, format!("{n}/{d}")),
        Number::Single(f) => Value::float(f64::from(f)),
        Number::Double(d) => Value::float(d),
    };
    Ok(Owner::take(req, scalar))
}

/// `exif_thumbnail($file, &$width = null, &$height = null, &$image_type
/// = null): string|false`: the embedded thumbnail, with its size and
/// type.
///
/// ```php
/// $jpeg = "\xFF\xD8\xFF\xC0\x00\x0B\x08\x00\x02\x00\x03\x01\x01\x11\x00\xFF\xD9";
/// $app1 = "Exif\0\0" . "MM\0\x2A\0\0\0\x08" . "\0\0" . "\0\0\0\x0E" . "\0\x02" . "\x02\x01\0\x04\0\0\0\x01\0\0\0\x2C" . "\x02\x02\0\x04\0\0\0\x01\0\0\0" . chr(strlen($jpeg)) . "\0\0\0\0" . $jpeg;
/// $file = "\xFF\xD8\xFF\xE1" . pack('n', strlen($app1) + 2) . $app1 . "\xFF\xDA\x00\x02";
/// $f = fopen('php://memory', 'w+');
/// fwrite($f, $file);
/// var_dump(bin2hex(exif_thumbnail($f, $width, $height, $type)), $width, $height, $type);
/// ```
/// ```expect
/// string(34) "ffd8ffc0000b080002000301011100ffd9"
/// int(3)
/// int(2)
/// int(2)
/// ```
#[php_function]
fn exif_thumbnail(
    req: &mut Request,
    file: Untyped<Value>,
    #[php(default = null)] width: Ref<Untyped<Value>>,
    #[php(default = null)] height: Ref<Untyped<Value>>,
    #[php(default = null)] image_type: Ref<Untyped<Value>>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Untyped(file) = file;
    let (mut reader, ok) = read(req, &file, true)?;
    let thumb = &reader.img.thumbnail;
    let data = match thumb.data.clone() {
        Some(data) if ok && thumb.size != 0 => data,
        _ => return Ok(OrFalse::False),
    };
    let size = usize::try_from(thumb.size).unwrap_or(0).min(data.len());
    let answer = PhpString::new(reader.req, &data[..size])?;
    if (width.is_given() || height.is_given())
        && (thumb.width == 0 || thumb.height == 0)
        && !reader.scan_thumbnail()
    {
        reader.img.thumbnail.width = 0;
        reader.img.thumbnail.height = 0;
    }
    let thumb = &reader.img.thumbnail;
    #[expect(clippy::cast_possible_wrap, reason = "php's `zend_long` of a `size_t`")]
    let answers = [
        (width, thumb.width as i64),
        (height, thumb.height as i64),
        (image_type, thumb.filetype.code()),
    ];
    for (slot, value) in answers {
        if let Err(thrown) = slot.set(reader.req, Untyped(Value::int(value))) {
            answer.give_up(reader.req);
            return Err(thrown);
        }
    }
    Ok(OrFalse::Value(answer))
}

/// `exif_imagetype(string $filename): int|false`: `php_getimagetype`
/// over the file.
///
/// ```php
/// var_dump(exif_imagetype('data://text/plain;base64,' . base64_encode("GIF89a\x01\x00")), exif_imagetype('data://text/plain,ab'), exif_imagetype('/nonexistent'));
/// ```
/// ```expect
/// Notice: exif_imagetype(): Error reading from data://text/plain,ab! in %s on line 2
///
/// Warning: exif_imagetype(/nonexistent): Failed to open stream: No such file or directory in %s on line 2
/// int(1)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn exif_imagetype(req: &mut Request, filename: Path) -> Result<OrFalse<i64>, Thrown> {
    let Path(filename) = filename;
    let opening = Opening {
        warn: true,
        ..Opening::default()
    };
    let Some(stream) = req.open_stream_with(filename.as_bytes(), b"rb", opening)? else {
        return Ok(OrFalse::False);
    };
    let bytes = stream.copy_to_mem(req, None);
    stream.free(req);
    let bytes = bytes?;
    let itype = php_ext_standard::php_getimagetype(req, bytes.as_bytes(), filename.as_bytes());
    bytes.give_up(req);
    Ok(match itype {
        0 => OrFalse::False,
        itype => OrFalse::Value(itype),
    })
}

/// `S_IFMT`.
const S_IFMT: i64 = 0o170_000;
/// `S_IFREG`.
const S_IFREG: i64 = 0o100_000;

/// `exif_read_from_file` and `exif_read_from_stream` over
/// `exif_read_from_impl`: the stream the argument names, its file's
/// facts, and the scan of its header through it. A stream handed in is
/// put back where it stood unless that was its start, where php leaves
/// it as the scan did; a path's is closed.
fn read<'r>(
    req: &'r mut Request,
    file: &Value,
    read_thumbnail: bool,
) -> Result<(Reader<'r>, bool), Thrown> {
    let mut img = Image::new(read_thumbnail, encodings(req));
    let (stream, owned) = if let ValueRef::Resource(_) = file.as_ref() {
        let Some(stream) = Stream::from_value(req, file) else {
            let function = req.active_function_name();
            let message = format!("{function}(): supplied resource is not a valid stream resource");
            return Err(throw_error(req, "TypeError", &message));
        };
        (Some(stream), None)
    } else {
        let path = to_string(req, file)?;
        let refused = if path.is_empty() {
            Some("not be empty")
        } else if path.as_bytes().contains(&0) {
            Some("not contain any null bytes")
        } else {
            None
        };
        if let Some(refused) = refused {
            path.give_up(req);
            return Err(arg_value_error(req, 1, refused));
        }
        let opening = Opening {
            must_seek: true,
            ..Opening::default()
        };
        let stream = req.open_stream_with(path.as_bytes(), b"rb", opening);
        path.give_up(req);
        let owned = stream?;
        (owned.as_deref().copied(), owned)
    };
    let opened = owned.is_some();
    let Some(stream) = stream else {
        let mut reader = Reader::new(req, img, None);
        reader.error(Level::Warning, "Unable to open file");
        return Ok((reader, false));
    };
    let old_pos = if opened { 0 } else { stream.tell(req) };
    if old_pos != 0 {
        stream.seek(req, 0, Whence::Set);
    }
    let mut not_a_file = false;
    if stream.is_stdio(req) {
        if let (Some(path), Some(stat)) = (stream.orig_path(req), stream.stat(req)) {
            if stat[2] & S_IFMT == S_IFREG {
                img.file_name = Some(php_ext_standard::php_basename(&path, b"").to_vec());
                img.file_date_time = stat[9];
                img.file_size = u64::try_from(stat[7]).unwrap_or(0);
            } else {
                not_a_file = true;
            }
        }
    } else {
        stream.seek(req, 0, Whence::End);
        img.file_size = u64::try_from(stream.tell(req)).unwrap_or(0);
        stream.seek(req, 0, Whence::Set);
    }
    let mut reader = Reader::new(req, img, Some(stream));
    let ok = if not_a_file {
        reader.error(Level::Warning, "Not a file");
        false
    } else {
        reader.scan()
    };
    if let Some(owned) = owned {
        owned.close(reader.req);
        owned.give_up(reader.req);
    } else if old_pos != 0 {
        stream.seek(reader.req, old_pos, Whence::Set);
    }
    if let Some(thrown) = reader.thrown.take() {
        return Err(thrown);
    }
    Ok((reader, ok))
}
