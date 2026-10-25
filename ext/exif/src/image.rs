//! What `exif.c` collects while it reads a file, its `image_info_type`:
//! the fourteen sections of values, the computed facts, the thumbnail,
//! and the byte order and format vocabulary every read goes through.

use php_ext::coerce::c_str;
use php_ext::raising::Thrown;
use php_ext::request::Request;

use crate::held::{Held, INFO_VALUE};

/// php's `motorola_intel`: the byte order a TIFF header declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Order {
    /// `II`, `motorola_intel = 0`.
    Intel,
    /// `MM`, `motorola_intel = 1`.
    Motorola,
}

impl Order {
    /// `N` bytes of `bytes` from `at`, zeros past its end: every read
    /// php makes is behind a bounds check of its own, so a zero here is
    /// a byte no read of php's reaches.
    pub(crate) fn take<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
        let mut out = [0; N];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = at
                .checked_add(i)
                .and_then(|i| bytes.get(i))
                .copied()
                .unwrap_or(0);
        }
        out
    }

    /// `php_ifd_get16u`.
    pub(crate) fn u16(self, bytes: &[u8], at: usize) -> u16 {
        let b = Order::take::<2>(bytes, at);
        match self {
            Order::Intel => u16::from_le_bytes(b),
            Order::Motorola => u16::from_be_bytes(b),
        }
    }

    /// `php_ifd_get32u`.
    pub(crate) fn u32(self, bytes: &[u8], at: usize) -> u32 {
        let b = Order::take::<4>(bytes, at);
        match self {
            Order::Intel => u32::from_le_bytes(b),
            Order::Motorola => u32::from_be_bytes(b),
        }
    }

    /// `php_ifd_get32s`, the same bits as C's `int`.
    pub(crate) fn i32(self, bytes: &[u8], at: usize) -> i32 {
        i32::from_ne_bytes(self.u32(bytes, at).to_ne_bytes())
    }

    /// `php_ifd_set16u`.
    pub(crate) fn put16(self, value: u16) -> [u8; 2] {
        match self {
            Order::Intel => value.to_le_bytes(),
            Order::Motorola => value.to_be_bytes(),
        }
    }

    /// `php_ifd_set32u`.
    pub(crate) fn put32(self, value: u32) -> [u8; 4] {
        match self {
            Order::Intel => value.to_le_bytes(),
            Order::Motorola => value.to_be_bytes(),
        }
    }
}

/// `php_ifd_get_float`: always little-endian, as php reads it whatever
/// the file's order.
pub(crate) fn get_float(bytes: &[u8], at: usize) -> f32 {
    f32::from_le_bytes(Order::take::<4>(bytes, at))
}

/// `php_ifd_get_double`: always little-endian, as [`get_float`].
pub(crate) fn get_double(bytes: &[u8], at: usize) -> f64 {
    f64::from_le_bytes(Order::take::<8>(bytes, at))
}

/// A directory entry's format, `TAG_FMT_*`, decoded once from its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Byte,
    String,
    UShort,
    ULong,
    URational,
    SByte,
    Undefined,
    SShort,
    SLong,
    SRational,
    Single,
    Double,
    Ifd,
}

impl Format {
    /// The format `code` names, `None` for `0` and past `NUM_FORMATS`.
    pub(crate) fn decode(code: u16) -> Option<Format> {
        Some(match code {
            1 => Format::Byte,
            2 => Format::String,
            3 => Format::UShort,
            4 => Format::ULong,
            5 => Format::URational,
            6 => Format::SByte,
            7 => Format::Undefined,
            8 => Format::SShort,
            9 => Format::SLong,
            10 => Format::SRational,
            11 => Format::Single,
            12 => Format::Double,
            13 => Format::Ifd,
            _ => return None,
        })
    }

    /// `TAG_FMT_*`.
    pub(crate) fn code(self) -> u16 {
        match self {
            Format::Byte => 1,
            Format::String => 2,
            Format::UShort => 3,
            Format::ULong => 4,
            Format::URational => 5,
            Format::SByte => 6,
            Format::Undefined => 7,
            Format::SShort => 8,
            Format::SLong => 9,
            Format::SRational => 10,
            Format::Single => 11,
            Format::Double => 12,
            Format::Ifd => 13,
        }
    }

    /// `php_tiff_bytes_per_format`.
    pub(crate) fn size(self) -> usize {
        match self {
            Format::Byte | Format::String | Format::SByte | Format::Undefined => 1,
            Format::UShort | Format::SShort => 2,
            Format::ULong | Format::SLong | Format::Single | Format::Ifd => 4,
            Format::URational | Format::SRational | Format::Double => 8,
        }
    }

    /// `exif_rewrite_tag_format_to_unsigned`.
    pub(crate) fn unsigned(self) -> Format {
        match self {
            Format::SByte => Format::Byte,
            Format::SRational => Format::URational,
            Format::SShort => Format::UShort,
            Format::SLong => Format::ULong,
            other => other,
        }
    }

    /// `exif_convert_any_format`: the number at `bytes`, as a double.
    pub(crate) fn to_double(self, bytes: &[u8], order: Order) -> f64 {
        match self {
            Format::SByte => f64::from(i8::from_ne_bytes(Order::take::<1>(bytes, 0))),
            Format::Byte => f64::from(Order::take::<1>(bytes, 0)[0]),
            Format::UShort => f64::from(order.u16(bytes, 0)),
            Format::ULong => f64::from(order.u32(bytes, 0)),
            Format::URational => match order.u32(bytes, 4) {
                0 => 0.0,
                den => f64::from(order.u32(bytes, 0)) / f64::from(den),
            },
            Format::SRational => match order.i32(bytes, 4) {
                0 => 0.0,
                den => f64::from(order.i32(bytes, 0)) / f64::from(den),
            },
            Format::SShort => f64::from(i16::from_ne_bytes(order.u16(bytes, 0).to_ne_bytes())),
            Format::SLong => f64::from(order.i32(bytes, 0)),
            Format::Single => f64::from(get_float(bytes, 0)),
            Format::Double => get_double(bytes, 0),
            Format::String | Format::Undefined | Format::Ifd => 0.0,
        }
    }

    /// `exif_convert_any_to_int`: the number at `bytes` as the `size_t`
    /// php returns, a negative `int` sign-extended into it.
    #[expect(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "C's conversions of `int` and `float` to `size_t`, which php's values keep"
    )]
    pub(crate) fn to_size(self, bytes: &[u8], order: Order) -> u64 {
        let saturate = |x: f64| {
            if x.is_nan() || x < 0.0 {
                0
            } else if x > u64::MAX as f64 {
                u64::MAX
            } else {
                x as u64
            }
        };
        match self {
            Format::SByte => i64::from(i8::from_ne_bytes(Order::take::<1>(bytes, 0))) as u64,
            Format::Byte => u64::from(Order::take::<1>(bytes, 0)[0]),
            Format::UShort | Format::SShort => u64::from(order.u16(bytes, 0)),
            Format::ULong | Format::SLong => u64::from(order.u32(bytes, 0)),
            Format::URational => match order.u32(bytes, 4) {
                0 => 0,
                den => u64::from(order.u32(bytes, 0) / den),
            },
            Format::SRational => match (order.i32(bytes, 0), order.i32(bytes, 4)) {
                (_, 0) => 0,
                (i32::MIN, -1) => i32::MAX as u64,
                (num, den) => i64::from(num / den) as u64,
            },
            Format::Single => saturate(f64::from(get_float(bytes, 0))),
            Format::Double => saturate(get_double(bytes, 0)),
            Format::String | Format::Undefined | Format::Ifd => 0,
        }
    }
}

/// One number of a numeric value, `image_info_value` as its format
/// fills it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Number {
    /// `u`, of `USHORT` and `ULONG`.
    Unsigned(u32),
    /// `i`, of `SSHORT` and `SLONG`.
    Signed(i32),
    /// `ur`.
    URational(u32, u32),
    /// `sr`.
    SRational(i32, i32),
    /// `f`.
    Single(f32),
    /// `d`.
    Double(f64),
}

/// What one value holds, as `add_assoc_image_info` renders it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stored {
    /// `BYTE`, `SBYTE`, `UNDEFINED` and `IFD`: the bytes, a PHP string.
    Bytes(Vec<u8>),
    /// `STRING`: a C string, cut at its first NUL.
    Text(Vec<u8>),
    /// The numeric formats: one number, or a list of them.
    Numbers(Vec<Number>),
}

/// One value of a section, `image_info_data`: the tag it came from, its
/// format and count as the directory gave them, the key it is stored
/// under, and the value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    /// The tag, as the `WORD` php keeps: `TAG_NONE` is `0xFFFF`.
    pub(crate) tag: u16,
    pub(crate) format: Format,
    /// The count, `0` for a value php renders as `NULL`.
    pub(crate) length: usize,
    pub(crate) name: Vec<u8>,
    pub(crate) value: Stored,
}

/// `TAG_NONE` as a `WORD`.
pub(crate) const TAG_NONE: u16 = 0xFFFF;
/// `TAG_COMPUTED_VALUE` as a `WORD`.
pub(crate) const TAG_COMPUTED: u16 = 0xFFFE;

/// The sections, `SECTION_*`, in php's numbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    File,
    Computed,
    AnyTag,
    Ifd0,
    Thumbnail,
    Comment,
    App0,
    Exif,
    Fpix,
    Gps,
    Interop,
    App12,
    WinXp,
    MakerNote,
}

impl Section {
    /// Every section, in `SECTION_*` order.
    pub(crate) const ALL: [Section; 14] = [
        Section::File,
        Section::Computed,
        Section::AnyTag,
        Section::Ifd0,
        Section::Thumbnail,
        Section::Comment,
        Section::App0,
        Section::Exif,
        Section::Fpix,
        Section::Gps,
        Section::Interop,
        Section::App12,
        Section::WinXp,
        Section::MakerNote,
    ];

    /// `SECTION_*`.
    pub(crate) fn index(self) -> usize {
        self as usize
    }

    /// `exif_get_sectionname`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Section::File => "FILE",
            Section::Computed => "COMPUTED",
            Section::AnyTag => "ANY_TAG",
            Section::Ifd0 => "IFD0",
            Section::Thumbnail => "THUMBNAIL",
            Section::Comment => "COMMENT",
            Section::App0 => "APP0",
            Section::Exif => "EXIF",
            Section::Fpix => "FPIX",
            Section::Gps => "GPS",
            Section::Interop => "INTEROP",
            Section::App12 => "APP12",
            Section::WinXp => "WINXP",
            Section::MakerNote => "MAKERNOTE",
        }
    }

    /// `exif_get_tag_table`.
    pub(crate) fn table(self) -> crate::tags::Table {
        match self {
            Section::Gps => crate::tags::GPS,
            Section::Interop => crate::tags::IOP,
            _ => crate::tags::IFD,
        }
    }
}

/// `sections_found`: the sections a read met, `FOUND_*`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Found(u16);

impl Found {
    pub(crate) fn add(&mut self, section: Section) {
        self.0 |= 1 << section.index();
    }

    pub(crate) fn has(self, section: Section) -> bool {
        self.0 & (1 << section.index()) != 0
    }

    /// Whether any section is in both.
    pub(crate) fn meets(self, other: Found) -> bool {
        self.0 & other.0 != 0
    }

    /// Whether any section but `COMPUTED` is found.
    pub(crate) fn beyond_computed(self) -> bool {
        self.0 & !(1 << Section::Computed.index()) != 0
    }

    /// `exif_get_sectionlist`: the names, comma separated.
    pub(crate) fn list(self) -> String {
        Section::ALL
            .iter()
            .filter(|s| self.has(**s))
            .map(|s| s.name())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The `image_filetype` values exif reads or reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileType {
    Unknown,
    Jpeg,
    TiffIntel,
    TiffMotorola,
    Heif,
}

impl FileType {
    /// `IMAGE_FILETYPE_*`.
    pub(crate) fn code(self) -> i64 {
        match self {
            FileType::Unknown => 0,
            FileType::Jpeg => 2,
            FileType::TiffIntel => 7,
            FileType::TiffMotorola => 8,
            FileType::Heif => 20,
        }
    }

    pub(crate) fn is_tiff(self) -> bool {
        matches!(self, FileType::TiffIntel | FileType::TiffMotorola)
    }
}

/// `thumbnail_data`.
#[derive(Debug, Clone)]
pub(crate) struct Thumbnail {
    pub(crate) filetype: FileType,
    pub(crate) width: u64,
    pub(crate) height: u64,
    pub(crate) size: u64,
    pub(crate) offset: u64,
    pub(crate) data: Option<Vec<u8>>,
}

/// The encoding names the `exif.*` ini entries held as the read began,
/// which `exif_read_from_impl` copies into the image.
#[derive(Debug, Clone, Default)]
pub(crate) struct EncodingNames {
    pub(crate) encode_unicode: Vec<u8>,
    pub(crate) decode_unicode_be: Vec<u8>,
    pub(crate) decode_unicode_le: Vec<u8>,
    pub(crate) encode_jis: Vec<u8>,
    pub(crate) decode_jis_be: Vec<u8>,
    pub(crate) decode_jis_le: Vec<u8>,
}

/// `image_info_type`, less its stream and its file sections, which the
/// reader holds.
#[derive(Debug)]
pub(crate) struct Image {
    /// `FileName`: the path's base name, for a plain file.
    pub(crate) file_name: Option<Vec<u8>>,
    pub(crate) file_date_time: i64,
    pub(crate) file_size: u64,
    pub(crate) file_type: FileType,
    pub(crate) height: i32,
    pub(crate) width: i32,
    pub(crate) is_color: bool,
    /// `make`, as a C string.
    pub(crate) make: Option<Vec<u8>>,
    pub(crate) aperture_f_number: f32,
    pub(crate) exposure_time: f32,
    pub(crate) focalplane_units: f64,
    pub(crate) ccd_width: f32,
    pub(crate) focalplane_x_res: f64,
    pub(crate) exif_image_width: u64,
    pub(crate) distance: f32,
    /// `motorola_intel`, `None` while no header has declared one (`-1`).
    pub(crate) order: Option<Order>,
    /// `UserComment`, its bytes as long as `UserCommentLength` says.
    pub(crate) user_comment: Option<Vec<u8>>,
    pub(crate) user_comment_encoding: Option<Vec<u8>>,
    pub(crate) encodings: EncodingNames,
    pub(crate) copyright: Option<Vec<u8>>,
    pub(crate) copyright_photographer: Option<Vec<u8>>,
    pub(crate) copyright_editor: Option<Vec<u8>>,
    /// `xp_fields`: each Windows XP tag with its decoded value, `None`
    /// for an empty one.
    pub(crate) xp_fields: Vec<(u16, Option<Vec<u8>>)>,
    pub(crate) thumbnail: Thumbnail,
    pub(crate) found: Found,
    pub(crate) sections: [Vec<Entry>; 14],
    pub(crate) read_thumbnail: bool,
    pub(crate) ifd_nesting_level: u32,
    pub(crate) ifd_count: u32,
    pub(crate) num_errors: u32,
    /// What php's image holds on the request's heap ([`Held`]).
    pub(crate) held: Held,
}

impl Image {
    /// The image `exif_read_from_impl` starts from.
    pub(crate) fn new(read_thumbnail: bool, encodings: EncodingNames) -> Image {
        Image {
            file_name: None,
            file_date_time: 0,
            file_size: 0,
            file_type: FileType::Unknown,
            height: 0,
            width: 0,
            is_color: false,
            make: None,
            aperture_f_number: 0.0,
            exposure_time: 0.0,
            focalplane_units: 0.0,
            ccd_width: 0.0,
            focalplane_x_res: 0.0,
            exif_image_width: 0,
            distance: 0.0,
            order: None,
            user_comment: None,
            user_comment_encoding: None,
            encodings,
            copyright: None,
            copyright_photographer: None,
            copyright_editor: None,
            xp_fields: Vec::new(),
            thumbnail: Thumbnail {
                filetype: FileType::Unknown,
                width: 0,
                height: 0,
                size: 0,
                offset: 0,
                data: None,
            },
            found: Found::default(),
            sections: Default::default(),
            read_thumbnail,
            ifd_nesting_level: 0,
            ifd_count: 0,
            num_errors: 0,
            held: Held::default(),
        }
    }

    /// The order reads use: `motorola_intel`'s `-1` is nonzero, so C
    /// reads it as Motorola.
    pub(crate) fn order(&self) -> Order {
        self.order.unwrap_or(Order::Motorola)
    }

    fn push(&mut self, section: Section, entry: Entry) {
        self.sections[section.index()].push(entry);
        self.found.add(section);
    }

    /// `exif_iif_add_value`: `length` values of `format` from `value`,
    /// whose first `value_len` bytes are the directory's, with the
    /// entry, the name and the value's copy laid on the request's heap
    /// as php lays them; a list of numbers is asked for fallibly since
    /// 8.5.12, and where the memory limit refused it the entry is kept
    /// with no value. The `Err` is the memory limit's end.
    #[expect(
        clippy::too_many_arguments,
        reason = "php's `exif_iif_add_value` and the request"
    )]
    pub(crate) fn add_value(
        &mut self,
        req: &mut Request,
        section: Section,
        name: Vec<u8>,
        tag: u16,
        format: Format,
        length: usize,
        value: Option<&[u8]>,
    ) -> Result<Option<String>, Thrown> {
        self.held.entry(req, section.index())?;
        self.held.block(req, name.len() + 1)?;
        let order = self.order();
        let value_len = value.map_or(0, <[u8]>::len);
        let mut warning = None;
        let mut too_long = |length: usize| {
            if length > value_len {
                warning = Some(format!(
                    "length > value_len: {} > {value_len}",
                    c_int(length as u64)
                ));
                return true;
            }
            false
        };
        // php overwrites the tag, not the format, of a format it does not
        // store (`TAG_FMT_IFD`), which a thumbnail's rebuilt directory shows.
        let stored_tag = if format == Format::Ifd { 7 } else { tag };
        let (length, stored) = match format {
            Format::String => {
                let value = value.filter(|_| !too_long(length));
                let text = value.map_or(&[][..], |v| c_str(&v[..length]));
                self.held.block(req, text.len() + 1)?;
                (text.len(), Stored::Text(text.to_vec()))
            }
            Format::Byte | Format::SByte | Format::Ifd if length == 0 => {
                (0, Stored::Bytes(Vec::new()))
            }
            Format::Byte | Format::SByte | Format::Ifd | Format::Undefined => {
                if let Some(v) = value.filter(|_| !too_long(length)) {
                    let bytes = if tag == 0x927C {
                        c_str(&v[..length])
                    } else {
                        &v[..length]
                    };
                    self.held.block(req, bytes.len() + 1)?;
                    (bytes.len(), Stored::Bytes(bytes.to_vec()))
                } else {
                    self.held.block(req, 1)?;
                    (0, Stored::Bytes(Vec::new()))
                }
            }
            _ if length == 0 => (0, Stored::Numbers(Vec::new())),
            _ if length > 1
                && !self
                    .held
                    .block_fallibly(req, length.saturating_mul(INFO_VALUE))? =>
            {
                (0, Stored::Numbers(Vec::new()))
            }
            _ => {
                let bytes = value.unwrap_or(&[]);
                let mut numbers = Vec::with_capacity(length.min(bytes.len()));
                let size = format.size();
                for at in (0..length).map(|i| i * size) {
                    if bytes.len().saturating_sub(at) < size {
                        warning = Some(String::from("Value too short"));
                        break;
                    }
                    numbers.push(match format {
                        Format::UShort => Number::Unsigned(u32::from(order.u16(bytes, at))),
                        Format::ULong => Number::Unsigned(order.u32(bytes, at)),
                        Format::URational => {
                            Number::URational(order.u32(bytes, at), order.u32(bytes, at + 4))
                        }
                        Format::SShort => Number::Signed(i32::from(i16::from_ne_bytes(
                            order.u16(bytes, at).to_ne_bytes(),
                        ))),
                        Format::SLong => Number::Signed(order.i32(bytes, at)),
                        Format::SRational => {
                            Number::SRational(order.i32(bytes, at), order.i32(bytes, at + 4))
                        }
                        Format::Single => Number::Single(get_float(bytes, at)),
                        _ => Number::Double(get_double(bytes, at)),
                    });
                }
                // What php leaves unwritten past a short value reads as
                // zeros here.
                let zero = match format {
                    Format::URational => Number::URational(0, 0),
                    Format::SRational => Number::SRational(0, 0),
                    Format::Single => Number::Single(0.0),
                    Format::Double => Number::Double(0.0),
                    Format::SShort | Format::SLong => Number::Signed(0),
                    _ => Number::Unsigned(0),
                };
                numbers.resize(length, zero);
                (length, Stored::Numbers(numbers))
            }
        };
        self.push(
            section,
            Entry {
                tag: stored_tag,
                format,
                length,
                name,
                value: stored,
            },
        );
        Ok(warning)
    }

    /// `exif_iif_add_int`: the entry and the name on the heap.
    pub(crate) fn add_int(
        &mut self,
        req: &mut Request,
        section: Section,
        name: &str,
        value: i32,
    ) -> Result<(), Thrown> {
        self.held.entry(req, section.index())?;
        self.held.block(req, name.len() + 1)?;
        let entry = Entry {
            tag: TAG_NONE,
            format: Format::SLong,
            length: 1,
            name: name.as_bytes().to_vec(),
            value: Stored::Numbers(vec![Number::Signed(value)]),
        };
        self.push(section, entry);
        Ok(())
    }

    /// `exif_iif_add_str`: nothing for `None`; the entry, the name and
    /// the value's `estrdup` on the heap.
    pub(crate) fn add_str(
        &mut self,
        req: &mut Request,
        section: Section,
        name: &[u8],
        value: Option<&[u8]>,
    ) -> Result<(), Thrown> {
        if let Some(value) = value {
            self.held.entry(req, section.index())?;
            self.held.block(req, name.len() + 1)?;
            self.held.block(req, c_str(value).len() + 1)?;
            let entry = Entry {
                tag: TAG_NONE,
                format: Format::String,
                length: 1,
                name: name.to_vec(),
                value: Stored::Text(c_str(value).to_vec()),
            };
            self.push(section, entry);
        }
        Ok(())
    }

    /// `exif_iif_add_buffer`: the entry, the name and `safe_emalloc(length,
    /// 1, 1)` on the heap.
    pub(crate) fn add_buffer(
        &mut self,
        req: &mut Request,
        section: Section,
        name: &str,
        value: &[u8],
    ) -> Result<(), Thrown> {
        self.held.entry(req, section.index())?;
        self.held.block(req, name.len() + 1)?;
        self.held.block(req, value.len() + 1)?;
        let entry = Entry {
            tag: TAG_NONE,
            format: Format::Undefined,
            length: value.len(),
            name: name.as_bytes().to_vec(),
            value: Stored::Bytes(value.to_vec()),
        };
        self.push(section, entry);
        Ok(())
    }
}

/// A `size_t` through a `%d`: its low 32 bits as C's `int`.
#[expect(clippy::cast_possible_truncation, reason = "C's `%d` of a `size_t`")]
pub(crate) fn c_int(value: u64) -> i32 {
    (value as u32).cast_signed()
}

/// A `size_t` through a `%X`: its low 32 bits.
#[expect(clippy::cast_possible_truncation, reason = "C's `%X` of a `size_t`")]
pub(crate) fn c_hex(value: u64) -> u32 {
    value as u32
}
