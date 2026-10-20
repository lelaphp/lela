//! `exif.c`'s reader: the file's header, the JPEG marker stream, the
//! TIFF and HEIF containers, the IFD walker with its maker notes, and
//! the thumbnail, each function php's own in php's order. Every offset
//! a directory gives is checked against a [`Window`] before a byte of
//! it is read, as php's `exif_offset_info` guards check it.

use php_ext::coerce::c_str;
use php_ext::raising::{Level, Thrown, raise};
use php_ext::request::{Request, Stream, Whence};

use crate::image::{
    FileType, Format, Found, Image, Order, Section, TAG_COMPUTED, TAG_NONE, c_hex, c_int,
};
use crate::tags::{self, MAKER_NOTES, NoteLayout, NoteOffsets, NoteOrder, Table};

/// `MAX_IFD_NESTING_LEVEL`.
const MAX_IFD_NESTING_LEVEL: u32 = 10;
/// `MAX_IFD_TAGS`.
const MAX_IFD_TAGS: u32 = 1000;
/// `EXIF_MAX_ERRORS`.
const MAX_ERRORS: u32 = 10;

/// `EXIF_ERROR_FILEEOF`.
const FILE_EOF: &str = "Unexpected end of file reached";
/// `EXIF_ERROR_CORRUPT`.
const CORRUPT: &str = "File structure corrupted";
/// `EXIF_ERROR_THUMBEOF`.
const THUMB_EOF: &str = "Thumbnail goes IFD boundary or end of file reached";

/// The tags the reader acts on, `TAG_*`.
mod tag {
    pub(super) const IMAGEWIDTH: u16 = 0x0100;
    pub(super) const IMAGEHEIGHT: u16 = 0x0101;
    pub(super) const PHOTOMETRIC_INTERPRETATION: u16 = 0x0106;
    pub(super) const MAKE: u16 = 0x010F;
    pub(super) const STRIP_OFFSETS: u16 = 0x0111;
    pub(super) const STRIP_BYTE_COUNTS: u16 = 0x0117;
    pub(super) const SUB_IFD: u16 = 0x014A;
    pub(super) const JPEG_INTERCHANGE_FORMAT: u16 = 0x0201;
    pub(super) const JPEG_INTERCHANGE_FORMAT_LEN: u16 = 0x0202;
    pub(super) const COPYRIGHT: u16 = 0x8298;
    pub(super) const EXPOSURETIME: u16 = 0x829A;
    pub(super) const FNUMBER: u16 = 0x829D;
    pub(super) const EXIF_IFD_POINTER: u16 = 0x8769;
    pub(super) const GPS_IFD_POINTER: u16 = 0x8825;
    pub(super) const SHUTTERSPEED: u16 = 0x9201;
    pub(super) const APERTURE: u16 = 0x9202;
    pub(super) const MAX_APERTURE: u16 = 0x9205;
    pub(super) const SUBJECT_DISTANCE: u16 = 0x9206;
    pub(super) const MAKER_NOTE: u16 = 0x927C;
    pub(super) const USERCOMMENT: u16 = 0x9286;
    pub(super) const XP_TITLE: u16 = 0x9C9B;
    pub(super) const XP_SUBJECT: u16 = 0x9C9F;
    pub(super) const COMP_IMAGE_WIDTH: u16 = 0xA002;
    pub(super) const COMP_IMAGE_HEIGHT: u16 = 0xA003;
    pub(super) const INTEROP_IFD_POINTER: u16 = 0xA005;
    pub(super) const FOCALPLANE_X_RES: u16 = 0xA20E;
    pub(super) const FOCALPLANE_RESOLUTION_UNIT: u16 = 0xA210;
}

/// The JPEG markers the scanner acts on, `M_*`.
mod marker {
    pub(super) const SOS: u16 = 0xDA;
    pub(super) const EOI: u16 = 0xD9;
    pub(super) const COM: u16 = 0xFE;
    pub(super) const EXIF: u16 = 0xE1;
    pub(super) const APP12: u16 = 0xEC;
    /// `M_PSEUDO`, no marker.
    pub(super) const PSEUDO: u16 = 0x123;

    /// `M_SOF0` to `M_SOF15` less `DHT`, `JPG` and `DAC`.
    pub(super) fn is_sof(marker: u16) -> bool {
        matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF)
    }
}

/// `exif_offset_info`: the bytes a directory's offsets may reach, and
/// the offset their first byte stands at.
#[derive(Clone, Copy)]
struct Window<'a> {
    bytes: &'a [u8],
    origin: u64,
}

impl<'a> Window<'a> {
    /// `exif_offset_info_try_get`: `len` bytes at `offset`.
    fn get(&self, offset: u64, len: u64) -> Option<&'a [u8]> {
        let start = usize::try_from(offset.checked_sub(self.origin)?).ok()?;
        let end = start.checked_add(usize::try_from(len).ok()?)?;
        self.bytes.get(start..end)
    }

    /// Where `offset` stands in [`Window::bytes`], `try_get` with no
    /// length.
    fn at(&self, offset: u64) -> Option<usize> {
        let start = usize::try_from(offset.checked_sub(self.origin)?).ok()?;
        (start <= self.bytes.len()).then_some(start)
    }

    /// `exif_offset_info_contains`.
    fn contains(&self, start: usize, len: u64) -> bool {
        u64::try_from(start)
            .ok()
            .and_then(|s| s.checked_add(len))
            .is_some_and(|end| end <= self.bytes.len() as u64)
    }
}

/// php's `strncmp(id, value, id.len())` is zero: the bytes equal up to
/// the id's first NUL.
fn signed_by(id: &[u8], value: &[u8]) -> bool {
    for (at, &b) in id.iter().enumerate() {
        if value.get(at) != Some(&b) {
            return false;
        }
        if b == 0 {
            return true;
        }
    }
    true
}

/// A reader over one stream: the image it fills, the stream, php's
/// `infile`, read, sought and told as php reads it, and the request
/// its diagnostics are raised in.
pub(crate) struct Reader<'r> {
    pub(crate) req: &'r mut Request,
    pub(crate) img: Image,
    stream: Option<Stream>,
    /// What a read threw, the memory limit's fatal: the native's answer,
    /// after which the reader reads nothing and raises nothing.
    pub(crate) thrown: Option<Thrown>,
}

impl<'r> Reader<'r> {
    pub(crate) fn new(req: &'r mut Request, img: Image, stream: Option<Stream>) -> Reader<'r> {
        Reader {
            req,
            img,
            stream,
            thrown: None,
        }
    }

    /// `php_stream_seek(infile, to, SEEK_SET)` succeeded.
    fn seek(&mut self, to: u64) -> bool {
        match (self.stream, i64::try_from(to)) {
            (Some(stream), Ok(to)) if self.thrown.is_none() => {
                stream.seek(self.req, to, Whence::Set)
            }
            _ => false,
        }
    }

    /// `php_stream_tell`.
    fn tell(&mut self) -> u64 {
        self.stream.map_or(0, |stream| {
            u64::try_from(stream.tell(self.req)).unwrap_or(0)
        })
    }

    /// `php_stream_getc`: `None` at the end.
    fn getc(&mut self) -> Option<u8> {
        self.read(1).first().copied()
    }

    /// `exif_read_from_stream_file_looped`: up to `count` bytes, read
    /// until they are had or the stream has no more.
    fn read(&mut self, count: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let Some(stream) = self.stream else {
            return out;
        };
        while self.thrown.is_none() && (out.len() as u64) < count {
            let want = usize::try_from(count - out.len() as u64).unwrap_or(usize::MAX);
            match stream.read(self.req, want) {
                Ok(Some(chunk)) if !chunk.is_empty() => {
                    out.extend_from_slice(chunk.as_bytes());
                    chunk.give_up(self.req);
                }
                Ok(Some(chunk)) => {
                    chunk.give_up(self.req);
                    break;
                }
                Ok(None) => break,
                Err(thrown) => self.thrown = Some(thrown),
            }
        }
        out
    }

    /// `exif_error_docref`: the message behind the native and the file
    /// name, the eleventh and later swallowed after one note that they
    /// are.
    pub(crate) fn error(&mut self, level: Level, message: &str) {
        if self.thrown.is_some() {
            return;
        }
        self.img.num_errors += 1;
        let function = self.req.frames().active_function_name();
        if self.img.num_errors > MAX_ERRORS {
            if self.img.num_errors == MAX_ERRORS + 1 {
                let text =
                    format!("{function}(): Further exif parsing errors have been suppressed");
                raise(self.req, level, &text);
            }
            return;
        }
        let name = String::from_utf8_lossy(self.img.file_name.as_deref().unwrap_or_default());
        raise(self.req, level, &format!("{function}({name}): {message}"));
    }

    fn warn(&mut self, message: &str) {
        self.error(Level::Warning, message);
    }

    /// `exif_scan_FILE_header`.
    pub(crate) fn scan(&mut self) -> bool {
        self.img.file_type = FileType::Unknown;
        if self.img.file_size < 2 {
            let size = c_int(self.img.file_size);
            self.warn(&format!("File too small ({size})"));
            return false;
        }
        self.seek(0);
        let mut header = [0u8; 16];
        let two = self.read(2);
        let Some(first) = header.get_mut(..2).filter(|_| two.len() == 2) else {
            return false;
        };
        first.copy_from_slice(&two);
        if header[..2] == [0xFF, 0xD8] {
            self.img.file_type = FileType::Jpeg;
            if self.scan_jpeg() {
                return true;
            }
            self.warn("Invalid JPEG file");
            return false;
        }
        if self.img.file_size < 8 {
            return false;
        }
        let six = self.read(6);
        if six.len() != 6 {
            return false;
        }
        header[2..8].copy_from_slice(&six);
        let tiff = match &header[..4] {
            b"II\x2A\x00" => Some((FileType::TiffIntel, Order::Intel)),
            b"MM\x00\x2A" => Some((FileType::TiffMotorola, Order::Motorola)),
            _ => None,
        };
        if let Some((file_type, order)) = tiff {
            self.img.file_type = file_type;
            self.img.order = Some(order);
            self.img.found.add(Section::Ifd0);
            if self.ifd_in_tiff(u64::from(order.u32(&header, 4)), Section::Ifd0) {
                return true;
            }
            self.warn("Invalid TIFF file");
            return false;
        }
        let heif = self.img.file_size > 16 && &header[4..8] == b"ftyp" && {
            let brand = self.read(4);
            header[8..8 + brand.len()].copy_from_slice(&brand);
            brand.len() == 4 && matches!(&header[8..12], b"heic" | b"heix" | b"mif1")
        };
        if heif {
            if self.scan_heif(&mut header) {
                self.img.file_type = FileType::Heif;
                return true;
            }
            self.warn("Invalid HEIF file");
            return false;
        }
        self.warn("File not supported");
        false
    }

    /// `exif_scan_JPEG_header`: the markers up to `SOS` or `EOI`.
    fn scan_jpeg(&mut self) -> bool {
        let mut last = marker::PSEUDO;
        let mut comment_correction = 1;
        loop {
            // Some software does not count the length bytes of a COM
            // section; php accepts that.
            if last == marker::COM && comment_correction != 0 {
                comment_correction = 2;
            }
            let code = loop {
                let Some(byte) = self.getc() else {
                    self.warn(CORRUPT);
                    return false;
                };
                let mut code = u16::from(byte);
                if last == marker::COM && comment_correction > 0 {
                    if code == 0xFF {
                        last = marker::PSEUDO;
                    } else {
                        code = 0xFF;
                        comment_correction -= 1;
                    }
                }
                if code != 0xFF {
                    break code;
                }
            };
            if last == marker::COM {
                if comment_correction == 0 {
                    self.error(
                        Level::Notice,
                        "Image has corrupt COM section: some software set wrong length information",
                    );
                } else {
                    // php returns `M_EOI`, which reads as true.
                    return true;
                }
            }
            let fpos = self.tell();
            let Some(lh) = self.getc() else {
                self.warn(CORRUPT);
                return false;
            };
            let Some(ll) = self.getc() else {
                self.warn(CORRUPT);
                return false;
            };
            let itemlen = u64::from(u16::from_be_bytes([lh, ll]));
            if itemlen < 2 {
                self.warn(CORRUPT);
                return false;
            }
            let mut data = vec![lh, ll];
            let rest = self.read(itemlen - 2);
            if rest.len() as u64 != itemlen - 2 {
                let got = rest.len();
                let want = itemlen - 2;
                self.warn(&format!(
                    "Error reading from file: got=x{got:04X}(={got}) != itemlen-2=x{want:04X}(={want})"
                ));
                return false;
            }
            data.extend_from_slice(&rest);
            match code {
                marker::SOS => return true,
                marker::EOI => {
                    self.warn("No image in jpeg!");
                    return self.img.found.beyond_computed();
                }
                marker::COM => {
                    let text = data.get(2..).unwrap_or_default();
                    let len = text.len();
                    self.add_tag(
                        Section::Comment,
                        b"Comment".to_vec(),
                        TAG_COMPUTED,
                        Format::String,
                        len,
                        text,
                    );
                }
                marker::EXIF if !self.img.found.has(Section::Ifd0) => self.app1(&data, fpos),
                marker::APP12 => self.app12(&data),
                sof if marker::is_sof(sof) => {
                    if itemlen - 2 < 6 {
                        return false;
                    }
                    let (height, width) = sof_size(&data);
                    self.img.height = i32::from(height);
                    self.img.width = i32::from(width);
                    self.img.is_color = data.get(7) == Some(&3);
                }
                _ => {}
            }
            last = code;
        }
    }

    /// `exif_process_APP12`: Olympus's company and info strings.
    fn app12(&mut self, data: &[u8]) {
        let body = data.get(2..).unwrap_or_default();
        let company = c_str(body);
        if company.is_empty() {
            return;
        }
        let l1 = company.len();
        self.add_tag(
            Section::App12,
            b"Company".to_vec(),
            TAG_NONE,
            Format::String,
            l1,
            company,
        );
        if let Some(rest) = body.get(l1 + 1..).filter(|rest| !rest.is_empty()) {
            let info = c_str(rest);
            self.add_tag(
                Section::App12,
                b"Info".to_vec(),
                TAG_NONE,
                Format::String,
                info.len(),
                info,
            );
        }
    }

    /// `exif_process_APP1`: the Exif identifier, then its TIFF header.
    fn app1(&mut self, data: &[u8], displacement: u64) {
        if data.len() <= 8 || &data[2..8] != b"Exif\0\0" {
            self.warn("Incorrect APP1 Exif Identifier Code");
            return;
        }
        self.tiff_in_jpeg(&data[8..], displacement + 8);
    }

    /// `exif_process_TIFF_in_JPEG`.
    fn tiff_in_jpeg(&mut self, data: &[u8], displacement: u64) {
        if data.len() < 2 {
            self.warn("Missing TIFF alignment marker");
            return;
        }
        let order = match &data[..2] {
            b"II" => Order::Intel,
            b"MM" => Order::Motorola,
            _ => {
                self.warn("Invalid TIFF alignment marker");
                return;
            }
        };
        self.img.order = Some(order);
        if data.len() < 8 || order.u16(data, 2) != 0x2A || order.u32(data, 4) < 8 {
            self.warn("Invalid TIFF start (1)");
            return;
        }
        let first = order.u32(data, 4);
        let Some(first) = usize::try_from(first).ok().filter(|&at| at <= data.len()) else {
            self.warn("Invalid IFD start");
            return;
        };
        self.img.found.add(Section::Ifd0);
        let window = Window {
            bytes: data,
            origin: 0,
        };
        self.ifd_in_jpeg(first, &window, displacement, Section::Ifd0, 0);
        if self.img.focalplane_x_res != 0.0 {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_precision_loss,
                reason = "php's `float` CCDWidth"
            )]
            let ccd = (self.img.exif_image_width as f64 * self.img.focalplane_units
                / self.img.focalplane_x_res) as f32;
            self.img.ccd_width = ccd;
        }
    }

    /// `exif_process_IFD_in_JPEG`: the directory at `dir_start`, then
    /// the next one as the thumbnail's.
    fn ifd_in_jpeg(
        &mut self,
        dir_start: usize,
        info: &Window<'_>,
        displacement: u64,
        section: Section,
        from_tag: u16,
    ) -> bool {
        self.img.found.add(Section::Ifd0);
        if !info.contains(dir_start, 2) {
            self.warn("Illegal IFD size");
            return false;
        }
        let order = self.img.order();
        let entries = usize::from(order.u16(info.bytes, dir_start));
        if !info.contains(dir_start + 2, entries as u64 * 12) {
            let start = c_hex(dir_start as u64 + 2);
            let end = c_hex((dir_start + 2 + entries * 12) as u64);
            let len = c_hex(info.bytes.len() as u64);
            self.warn(&format!(
                "Illegal IFD size: x{start:04X} + 2 + x{entries:04X}*12 = x{end:04X} > x{len:04X}"
            ));
            return false;
        }
        for de in 0..entries {
            let entry = Order::take::<12>(info.bytes, dir_start + 2 + 12 * de);
            if !self.ifd_tag(entry, info, displacement, section, true, section.table()) {
                return false;
            }
        }
        if section == Section::Thumbnail {
            return true;
        }
        let next_at = dir_start + 2 + 12 * entries;
        if !info.contains(next_at, 4) {
            // A missing offset to the next IFD is no next IFD.
            return true;
        }
        let next = if from_tag == tag::EXIF_IFD_POINTER || from_tag == tag::GPS_IFD_POINTER {
            0
        } else {
            self.img.order().u32(info.bytes, next_at)
        };
        if next == 0 {
            return true;
        }
        let Some(next_start) = info.at(u64::from(next)) else {
            self.warn("Illegal IFD offset");
            return false;
        };
        if !self.ifd_in_jpeg(next_start, info, displacement, Section::Thumbnail, 0) {
            return false;
        }
        let thumb = &self.img.thumbnail;
        if thumb.filetype != FileType::Unknown
            && thumb.size != 0
            && thumb.offset != 0
            && self.img.read_thumbnail
        {
            self.thumbnail_extract(info);
        }
        true
    }

    /// `exif_process_IFD_TAG`: one entry, guarded against runaway
    /// directories.
    fn ifd_tag(
        &mut self,
        entry: [u8; 12],
        info: &Window<'_>,
        displacement: u64,
        section: Section,
        read_next: bool,
        table: Table,
    ) -> bool {
        let count = self.img.ifd_count;
        self.img.ifd_count += 1;
        if count > MAX_IFD_TAGS {
            self.warn("corrupt EXIF header: maximum IFD tag count reached");
            return false;
        }
        if self.img.ifd_nesting_level > MAX_IFD_NESTING_LEVEL {
            self.warn("corrupt EXIF header: maximum directory nesting level reached");
            return false;
        }
        self.img.ifd_nesting_level += 1;
        let result = self.ifd_tag_impl(entry, info, displacement, section, read_next, table);
        self.img.ifd_nesting_level -= 1;
        result
    }

    /// `REQUIRE_NON_EMPTY`'s warning.
    fn empty(&mut self, tag: u16, table: Table) -> bool {
        let name = tags::debug_name(tag, table);
        self.warn(&format!("Process tag(x{tag:04X}={name}): Cannot be empty"));
        false
    }

    /// `exif_process_IFD_TAG_impl`.
    #[expect(clippy::too_many_lines, reason = "php's one function, case by case")]
    fn ifd_tag_impl(
        &mut self,
        entry: [u8; 12],
        info: &Window<'_>,
        displacement: u64,
        section: Section,
        read_next: bool,
        table: Table,
    ) -> bool {
        let order = self.img.order();
        let tag = order.u16(&entry, 0);
        let code = order.u16(&entry, 2);
        let components = order.u32(&entry, 4);
        let format = Format::decode(code).unwrap_or_else(|| {
            let name = tags::debug_name(tag, table);
            self.warn(&format!(
                "Process tag(x{tag:04X}={name}): Illegal format code 0x{code:04X}, suppose BYTE"
            ));
            Format::Byte
        });
        let byte_count = u64::from(components) * format.size() as u64;
        if byte_count > i32::MAX as u64 {
            let name = tags::debug_name(tag, table);
            self.warn(&format!(
                "Process tag(x{tag:04X}={name}): Illegal byte_count"
            ));
            return false;
        }
        let count = usize::try_from(byte_count).unwrap_or(0);
        let read: Vec<u8>;
        let raw: &[u8] = if byte_count > 4 {
            let offset = u64::from(order.u32(&entry, 8));
            if let Some(value) = info.get(offset, byte_count) {
                value
            } else {
                // JPEG's offsets count from its TIFF header, the one
                // `displacement` stands at.
                let size = self.img.file_size;
                if byte_count > size
                    || offset > size - byte_count
                    || !(self.img.file_type.is_tiff() || self.img.file_type == FileType::Jpeg)
                {
                    let name = tags::debug_name(tag, table);
                    let (o, b, e, s) = (
                        c_hex(offset),
                        c_hex(byte_count),
                        c_hex(offset + byte_count),
                        c_hex(size),
                    );
                    self.warn(&format!(
                        "Process tag(x{tag:04X}={name}): Illegal pointer offset(x{o:04X} + x{b:04X} = x{e:04X} > x{s:04X})"
                    ));
                    return false;
                }
                let fpos = self.tell();
                let target = displacement + offset;
                self.seek(target);
                let at = self.tell();
                if at != target {
                    let (at, target) = (c_hex(at), c_hex(target));
                    self.warn(&format!("Wrong file pointer: 0x{at:08X} != 0x{target:08X}"));
                    return false;
                }
                read = self.read(byte_count);
                self.seek(fpos);
                if read.len() != count {
                    self.warn(FILE_EOF);
                    return false;
                }
                &read
            }
        } else {
            &entry[8..]
        };
        let mut value = std::borrow::Cow::Borrowed(raw);
        self.img.found.add(Section::AnyTag);
        if section == Section::Thumbnail {
            if self.img.thumbnail.data.is_none() {
                if byte_count == 0 {
                    return self.empty(tag, table);
                }
                let number = format.unsigned().to_size(&value, order);
                let thumb = &mut self.img.thumbnail;
                match tag {
                    tag::IMAGEWIDTH | tag::COMP_IMAGE_WIDTH => thumb.width = number,
                    tag::IMAGEHEIGHT | tag::COMP_IMAGE_HEIGHT => thumb.height = number,
                    tag::STRIP_OFFSETS | tag::JPEG_INTERCHANGE_FORMAT => thumb.offset = number,
                    tag::STRIP_BYTE_COUNTS => {
                        thumb.filetype = if self.img.file_type.is_tiff() {
                            self.img.file_type
                        } else {
                            FileType::TiffMotorola
                        };
                        thumb.size = number;
                    }
                    tag::JPEG_INTERCHANGE_FORMAT_LEN if thumb.filetype == FileType::Unknown => {
                        thumb.filetype = FileType::Jpeg;
                        thumb.size = number;
                    }
                    _ => {}
                }
            }
        } else if section == Section::Ifd0 || section == Section::Exif {
            let bytes = &value[..count.min(value.len())];
            match tag {
                tag::COPYRIGHT => self.copyright(bytes),
                tag::USERCOMMENT => {
                    let mut comment = bytes.to_vec();
                    let (text, encoding) = self.user_comment(&mut comment);
                    self.img.user_comment = text;
                    self.img.user_comment_encoding = encoding;
                    comment.extend_from_slice(raw.get(count..).unwrap_or_default());
                    value = std::borrow::Cow::Owned(comment);
                }
                tag::XP_TITLE..=tag::XP_SUBJECT => {
                    self.img.found.add(Section::WinXp);
                    let text = self.decode(bytes, Codec::Unicode);
                    self.img.xp_fields.push((tag, text));
                }
                tag::FNUMBER => {
                    if byte_count == 0 {
                        return self.empty(tag, table);
                    }
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "php's `float` ApertureFNumber"
                    )]
                    let f = format.to_double(&value, order) as f32;
                    self.img.aperture_f_number = f;
                }
                // Earlier, more direct values win over these.
                tag::APERTURE | tag::MAX_APERTURE if self.img.aperture_f_number == 0.0 => {
                    if byte_count == 0 {
                        return self.empty(tag, table);
                    }
                    let x = format.to_double(&value, order) * f64::from(2f32.ln()) * 0.5;
                    #[expect(clippy::cast_possible_truncation, reason = "php's `expf`")]
                    let f = (x as f32).exp();
                    self.img.aperture_f_number = f;
                }
                tag::SHUTTERSPEED if self.img.exposure_time == 0.0 => {
                    if byte_count == 0 {
                        return self.empty(tag, table);
                    }
                    let x = -format.to_double(&value, order) * f64::from(2f32.ln());
                    #[expect(clippy::cast_possible_truncation, reason = "php's `expf`")]
                    let f = (x as f32).exp();
                    self.img.exposure_time = f;
                }
                tag::EXPOSURETIME => self.img.exposure_time = -1.0,
                tag::COMP_IMAGE_WIDTH => {
                    if byte_count == 0 {
                        return self.empty(tag, table);
                    }
                    self.img.exif_image_width = format.unsigned().to_size(&value, order);
                }
                tag::FOCALPLANE_X_RES => {
                    if byte_count == 0 {
                        return self.empty(tag, table);
                    }
                    self.img.focalplane_x_res = format.to_double(&value, order);
                }
                tag::SUBJECT_DISTANCE => {
                    if byte_count == 0 {
                        return self.empty(tag, table);
                    }
                    #[expect(clippy::cast_possible_truncation, reason = "php's `float` Distance")]
                    let d = format.to_double(&value, order) as f32;
                    self.img.distance = d;
                }
                tag::FOCALPLANE_RESOLUTION_UNIT => {
                    if byte_count == 0 {
                        return self.empty(tag, table);
                    }
                    match format.to_size(&value, order) {
                        1 | 2 => self.img.focalplane_units = 25.4,
                        3 => self.img.focalplane_units = 10.0,
                        4 => self.img.focalplane_units = 1.0,
                        5 => self.img.focalplane_units = 0.001,
                        _ => {}
                    }
                }
                tag::SUB_IFD if format == Format::Ifd => self.error(Level::Notice, "Skip SUB IFD"),
                tag::MAKE => self.img.make = Some(c_str(bytes).to_vec()),
                tag::MAKER_NOTE if !self.maker_note(bytes, info, displacement) => return false,
                tag::EXIF_IFD_POINTER | tag::GPS_IFD_POINTER | tag::INTEROP_IFD_POINTER
                    if read_next =>
                {
                    if byte_count == 0 {
                        return self.empty(tag, table);
                    }
                    let sub = match tag {
                        tag::EXIF_IFD_POINTER => Section::Exif,
                        tag::GPS_IFD_POINTER => Section::Gps,
                        _ => Section::Interop,
                    };
                    self.img.found.add(sub);
                    let Some(start) = info.at(u64::from(order.u32(&value, 0))) else {
                        self.warn("Illegal IFD Pointer");
                        return false;
                    };
                    if !self.ifd_in_jpeg(start, info, displacement, sub, tag) {
                        return false;
                    }
                }
                _ => {}
            }
        }
        let name = tags::key(tag, table);
        let bytes = &value[..count.min(value.len())];
        self.add_tag(section, name, tag, format, components as usize, bytes);
        true
    }

    /// `exif_iif_add_tag`, its warning raised.
    fn add_tag(
        &mut self,
        section: Section,
        name: Vec<u8>,
        tag: u16,
        format: Format,
        length: usize,
        value: &[u8],
    ) {
        if let Some(warning) = self
            .img
            .add_value(section, name, tag, format, length, Some(value))
        {
            self.warn(&warning);
        }
    }

    /// `TAG_COPYRIGHT`: "photographer NUL editor NUL", or one string.
    fn copyright(&mut self, bytes: &[u8]) {
        let photographer = c_str(bytes);
        let length = photographer.len();
        if bytes.len() <= 1 || length == 0 {
            return;
        }
        if length < bytes.len() - 1 {
            let editor = c_str(&bytes[length + 1..]).to_vec();
            let mut both = photographer.to_vec();
            both.extend_from_slice(b", ");
            both.extend_from_slice(&editor);
            self.img.copyright_photographer = Some(photographer.to_vec());
            self.img.copyright_editor = Some(editor);
            self.img.copyright = Some(both);
        } else {
            self.img.copyright = Some(photographer.to_vec());
        }
    }

    /// `exif_process_user_comment`: the comment and the encoding its
    /// header names. An Olympus comment's trailing spaces become NULs in
    /// `value` itself, which the tag is then stored with, as php's are.
    fn user_comment(&mut self, value: &mut [u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let mut rest = &mut value[..];
        let mut encoding = None;
        if rest.len() >= 8 {
            let head = Order::take::<8>(rest, 0);
            let codec = match &head {
                b"UNICODE\0" => Some(Codec::Unicode),
                b"JIS\0\0\0\0\0" => Some(Codec::Jis),
                _ => None,
            };
            let named: Option<&[u8]> = match &head {
                b"ASCII\0\0\0" | b"UNICODE\0" | b"JIS\0\0\0\0\0" => Some(c_str(&head)),
                b"\0\0\0\0\0\0\0\0" => Some(b"UNDEFINED"),
                _ => None,
            };
            if let Some(named) = named {
                encoding = Some(named.to_vec());
                rest = &mut rest[8..];
            }
            if let Some(codec) = codec {
                let text = match (codec, rest.get(..2)) {
                    (Codec::Unicode, Some(b"\xFE\xFF")) => {
                        self.convert_or_raw(&rest[2..], Codec::Unicode, b"UCS-2BE")
                    }
                    (Codec::Unicode, Some(b"\xFF\xFE")) => {
                        self.convert_or_raw(&rest[2..], Codec::Unicode, b"UCS-2LE")
                    }
                    _ => self.decode(rest, codec),
                };
                return (text, encoding);
            }
        }
        if let Some(last) = rest.len().checked_sub(1) {
            let mut a = last;
            while a != 0 && rest[a] == b' ' {
                rest[a] = 0;
                a -= 1;
            }
        }
        (Some(c_str(rest).to_vec()), encoding)
    }

    /// `bytes` decoded from the file order's encoding of `codec`.
    fn decode(&mut self, bytes: &[u8], codec: Codec) -> Option<Vec<u8>> {
        let e = &self.img.encodings;
        let from = match (codec, self.img.order()) {
            (Codec::Unicode, Order::Motorola) => e.decode_unicode_be.clone(),
            (Codec::Unicode, Order::Intel) => e.decode_unicode_le.clone(),
            (Codec::Jis, Order::Motorola) => e.decode_jis_be.clone(),
            (Codec::Jis, Order::Intel) => e.decode_jis_le.clone(),
        };
        self.convert_or_raw(bytes, codec, &from)
    }

    /// `zend_multibyte_encoding_converter` into `codec`'s target, or
    /// `exif_process_string_raw` where it does not convert: the bytes
    /// and a NUL, none for no bytes.
    fn convert_or_raw(&mut self, bytes: &[u8], codec: Codec, from: &[u8]) -> Option<Vec<u8>> {
        let to = match codec {
            Codec::Unicode => self.img.encodings.encode_unicode.clone(),
            Codec::Jis => self.img.encodings.encode_jis.clone(),
        };
        if let Some(converted) = crate::decode::convert(self.req, bytes, &to, from) {
            return Some(converted);
        }
        (!bytes.is_empty()).then(|| {
            let mut raw = bytes.to_vec();
            raw.push(0);
            raw
        })
    }

    /// `exif_process_IFD_in_MAKERNOTE`: the maker's directory, if the
    /// camera is one php knows.
    fn maker_note(&mut self, value: &[u8], info: &Window<'_>, displacement: u64) -> bool {
        let len = value.len();
        let make = self.img.make.as_deref();
        let Some(note) = MAKER_NOTES.iter().find(|note| {
            make == Some(note.make)
                && note
                    .id
                    .is_none_or(|id| len < id.len() || signed_by(id, value))
        }) else {
            return true;
        };
        if len < 2 || note.offset >= len - 1 {
            let (l, o) = (c_hex(len as u64), note.offset);
            self.warn(&format!("IFD data too short: 0x{l:04X} offset 0x{o:04X}"));
            return true;
        }
        let NoteLayout::Tags(table) = note.layout else {
            return true;
        };
        self.img.found.add(Section::MakerNote);
        let old = self.img.order;
        match note.order {
            NoteOrder::Intel => self.img.order = Some(Order::Intel),
            NoteOrder::Motorola => self.img.order = Some(Order::Motorola),
            NoteOrder::Normal => {}
        }
        let mut entries = usize::from(self.img.order().u16(value, note.offset));
        if 2 + entries * 12 > len {
            self.error(
                Level::Notice,
                "Potentially invalid endianness, trying again with different endianness before imminent failure.",
            );
            self.img.order = Some(match self.img.order {
                Some(Order::Intel) => Order::Motorola,
                _ => Order::Intel,
            });
            entries = usize::from(self.img.order().u16(value, note.offset));
        }
        let size = 2 + entries * 12;
        if size > len {
            let (e, s, l) = (entries, c_hex(size as u64), c_hex(len as u64));
            self.warn(&format!(
                "Illegal IFD size: 2 + 0x{e:04X}*12 = 0x{s:04X} > 0x{l:04X}"
            ));
            return false;
        }
        if note.offset > len - size {
            let (s, l) = (c_hex((note.offset + size) as u64), c_hex(len as u64));
            self.warn(&format!("Illegal IFD size: 0x{s:04X} > 0x{l:04X}"));
            return false;
        }
        let own = Window {
            bytes: value,
            origin: 0,
        };
        let window = match note.offsets {
            NoteOffsets::Maker => &own,
            NoteOffsets::Normal => info,
        };
        for de in 0..entries {
            let entry = Order::take::<12>(value, note.offset + 2 + 12 * de);
            if !self.ifd_tag(
                entry,
                window,
                displacement,
                Section::MakerNote,
                false,
                table,
            ) {
                return false;
            }
        }
        self.img.order = old;
        true
    }

    /// `exif_thumbnail_extract`: the thumbnail's bytes from the window.
    fn thumbnail_extract(&mut self, info: &Window<'_>) {
        if self.img.thumbnail.data.is_some() {
            self.warn("Multiple possible thumbnails");
            return;
        }
        if !self.img.read_thumbnail {
            return;
        }
        let thumb = &self.img.thumbnail;
        if thumb.size >= 65536 || thumb.size == 0 || thumb.offset == 0 {
            self.warn("Illegal thumbnail size/offset");
            return;
        }
        let Some(bytes) = info.get(thumb.offset, thumb.size) else {
            self.warn(THUMB_EOF);
            return;
        };
        self.img.thumbnail.data = Some(bytes.to_vec());
        self.thumbnail_build();
    }

    /// `exif_thumbnail_build`: a TIFF thumbnail made a TIFF file of its
    /// own, its directory rebuilt from the `THUMBNAIL` section before
    /// its data. What php writes over its reallocated buffer is written
    /// here over the same bytes, so a gap it leaves keeps the old data.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "php's `WORD` tags and `DWORD` lengths and offsets"
    )]
    fn thumbnail_build(&mut self) {
        let img = &mut self.img;
        let thumb = &img.thumbnail;
        if !img.read_thumbnail || thumb.offset == 0 || thumb.size == 0 || !thumb.filetype.is_tiff()
        {
            return;
        }
        let Some(old) = img.thumbnail.data.take() else {
            return;
        };
        let order = img.order();
        let entries = &img.sections[Section::Thumbnail.index()];
        let byte_count = |e: &crate::image::Entry| e.format.size() * e.length;
        let mut new_value = 8 + 2 + entries.len() * 12 + 4;
        let new_move = new_value
            + entries
                .iter()
                .map(byte_count)
                .filter(|&n| n > 4)
                .sum::<usize>();
        let mut out = old.clone();
        out.resize(new_move + old.len(), 0);
        out[new_move..].copy_from_slice(&old);
        let mut put = |at: usize, bytes: &[u8]| {
            if let Some(slot) = out.get_mut(at..at + bytes.len()) {
                slot.copy_from_slice(bytes);
            }
        };
        put(
            0,
            match order {
                Order::Motorola => b"MM\x00\x2a\x00\x00\x00\x08",
                Order::Intel => b"II\x2a\x00\x08\x00\x00\x00",
            },
        );
        put(8, &order.put16(entries.len() as u16));
        let mut at = 10;
        for entry in entries {
            put(at, &order.put16(entry.tag));
            if entry.tag == tag::STRIP_OFFSETS || entry.tag == tag::JPEG_INTERCHANGE_FORMAT {
                put(at + 2, &order.put16(Format::ULong.code()));
                put(at + 4, &order.put32(1));
                put(at + 8, &order.put32(new_move as u32));
            } else {
                put(at + 2, &order.put16(entry.format.code()));
                put(at + 4, &order.put32(entry.length as u32));
                let bytes = ifd_value(entry, order);
                if bytes.len() <= 4 {
                    put(at + 8, &Order::take::<4>(&bytes, 0));
                } else {
                    put(at + 8, &order.put32(new_value as u32));
                    put(new_value, &bytes);
                    new_value += bytes.len();
                }
            }
            at += 12;
        }
        put(at, &[0; 4]);
        img.thumbnail.size += new_move as u64;
        img.thumbnail.data = Some(out);
    }

    /// `exif_process_IFD_in_TIFF`: guarded as [`Reader::ifd_tag`] is,
    /// without a word.
    fn ifd_in_tiff(&mut self, dir_offset: u64, section: Section) -> bool {
        let count = self.img.ifd_count;
        self.img.ifd_count += 1;
        if count > MAX_IFD_TAGS || self.img.ifd_nesting_level > MAX_IFD_NESTING_LEVEL {
            return false;
        }
        self.img.ifd_nesting_level += 1;
        let result = self.ifd_in_tiff_impl(dir_offset, section);
        self.img.ifd_nesting_level -= 1;
        result
    }

    /// `exif_process_IFD_in_TIFF_impl`: one directory of a TIFF file,
    /// read with the values that follow it, then its sub-directories and
    /// the next directory, the thumbnail's.
    #[expect(clippy::too_many_lines, reason = "php's one function")]
    fn ifd_in_tiff_impl(&mut self, dir_offset: u64, section: Section) -> bool {
        let size = self.img.file_size;
        let order = self.img.order();
        let table = section.table();
        if size < 2 || size - 2 < dir_offset {
            let at = c_hex(dir_offset + 2);
            let s = c_hex(size);
            self.warn(&format!(
                "Error in TIFF: filesize(x{s:04X}) less than start of IFD dir(x{at:04X})"
            ));
            return false;
        }
        self.seek(dir_offset);
        let mut dir = self.read(2);
        if dir.len() != 2 {
            return false;
        }
        let entries = usize::from(order.u16(&dir, 0));
        let dir_size = 2 + 12 * entries as u64 + 4;
        if size < dir_size || size - dir_size < dir_offset {
            let (s, at) = (c_hex(size), c_hex(dir_offset + dir_size));
            self.warn(&format!(
                "Error in TIFF: filesize(x{s:04X}) less than size of IFD dir(x{at:04X})"
            ));
            return false;
        }
        let rest = self.read(dir_size - 2);
        if rest.len() as u64 != dir_size - 2 {
            return false;
        }
        dir.extend_from_slice(&rest);
        let next_offset = order.u32(&dir, dir.len() - 4);
        let mut ifd_size = dir_size;
        // php keeps the last small value across entries.
        let mut value: u64 = 0;
        for i in 0..entries {
            let entry = Order::take::<12>(&dir, 2 + i * 12);
            let tag = order.u16(&entry, 0);
            let code = order.u16(&entry, 2);
            let size_of = if code > 13 {
                let name = tags::debug_name(tag, table);
                self.error(
                    Level::Notice,
                    &format!(
                        "Read from TIFF: tag(0x{tag:04X},{name:>12}): Illegal format code 0x{code:04X}, switching to BYTE"
                    ),
                );
                1
            } else {
                Format::decode(code).map_or(0, Format::size)
            };
            #[expect(clippy::cast_possible_truncation, reason = "php's `unsigned` product")]
            let length = u64::from(order.u32(&entry, 4).wrapping_mul(size_of as u32));
            if length <= 4 {
                #[expect(clippy::cast_sign_loss, reason = "php's `size_t` of a `signed short`")]
                match Format::decode(code) {
                    Some(Format::UShort) => value = u64::from(order.u16(&entry, 8)),
                    Some(Format::SShort) => {
                        value = i64::from(i16::from_ne_bytes(order.u16(&entry, 8).to_ne_bytes()))
                            as u64;
                    }
                    Some(Format::ULong | Format::SLong) => value = u64::from(order.u32(&entry, 8)),
                    _ => {}
                }
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "php's `int` Width and Height"
                )]
                match tag {
                    tag::IMAGEWIDTH | tag::COMP_IMAGE_WIDTH => self.img.width = value as i32,
                    tag::IMAGEHEIGHT | tag::COMP_IMAGE_HEIGHT => self.img.height = value as i32,
                    tag::PHOTOMETRIC_INTERPRETATION => match value {
                        0 | 1 | 4 => self.img.is_color = false,
                        2 | 3 | 5 | 6 | 8 => self.img.is_color = true,
                        _ => {}
                    },
                    _ => {}
                }
            } else {
                let offset = u64::from(order.u32(&entry, 8));
                if offset + length > dir_offset + ifd_size && offset == dir_offset + ifd_size {
                    ifd_size = offset + length - dir_offset;
                }
            }
        }
        if ifd_size > dir_size {
            if size < ifd_size || dir_offset > size - ifd_size {
                let (s, o, i) = (c_hex(size), c_hex(dir_offset), c_hex(ifd_size));
                self.warn(&format!(
                    "Error in TIFF: filesize(x{s:04X}) less than size of IFD(x{o:04X} + x{i:04X})"
                ));
                return false;
            }
            let values = self.read(ifd_size - dir_size);
            dir.extend_from_slice(&values);
            dir.resize(usize::try_from(ifd_size).unwrap_or(0), 0);
        }
        for i in 0..entries {
            // A maker note that failed leaves its order behind, and php
            // reads on in it.
            let order = self.img.order();
            let entry = Order::take::<12>(&dir, 2 + i * 12);
            let tag = order.u16(&entry, 0);
            let sub = match tag {
                tag::EXIF_IFD_POINTER => Section::Exif,
                tag::INTEROP_IFD_POINTER => Section::Interop,
                tag::GPS_IFD_POINTER => Section::Gps,
                tag::SUB_IFD => Section::Thumbnail,
                _ => {
                    let window = Window {
                        bytes: &dir,
                        origin: dir_offset,
                    };
                    if !self.ifd_tag(entry, &window, 0, section, false, table) {
                        return false;
                    }
                    continue;
                }
            };
            self.img.found.add(sub);
            self.ifd_in_tiff(u64::from(order.u32(&entry, 8)), sub);
            let thumb = &self.img.thumbnail;
            if section != Section::Thumbnail
                && tag == tag::SUB_IFD
                && thumb.filetype != FileType::Unknown
                && thumb.size != 0
                && thumb.offset != 0
                && self.img.read_thumbnail
                && thumb.data.is_none()
            {
                self.thumbnail_from_file();
            }
        }
        if next_offset != 0 && section != Section::Thumbnail {
            self.ifd_in_tiff(u64::from(next_offset), Section::Thumbnail);
            let thumb = &self.img.thumbnail;
            if thumb.data.is_none()
                && thumb.offset != 0
                && thumb.size != 0
                && self.img.read_thumbnail
            {
                self.thumbnail_from_file();
            }
        }
        true
    }

    /// A TIFF thumbnail's bytes, read where its offset points.
    fn thumbnail_from_file(&mut self) {
        let (offset, size) = (self.img.thumbnail.offset, self.img.thumbnail.size);
        self.seek(offset);
        let data = self.read(size);
        if data.len() as u64 == size {
            self.img.thumbnail.data = Some(data);
            self.thumbnail_build();
        } else {
            self.warn(THUMB_EOF);
        }
    }

    /// `exif_scan_HEIF_header`: the `meta` box's `Exif` item, read as
    /// an APP1 segment.
    fn scan_heif(&mut self, buf: &mut [u8; 16]) -> bool {
        let size = self.img.file_size;
        let mut offset = u64::from(Order::Motorola.u32(buf, 0));
        while size - 16 > offset {
            let head = if self.seek(offset) {
                self.read(16)
            } else {
                Vec::new()
            };
            if head.len() != 16 {
                return false;
            }
            buf.copy_from_slice(&head);
            let (header, box_size, kind) = parse_box(buf);
            if box_size < header {
                return false;
            }
            if &kind == b"meta" {
                let limit = box_size - header;
                if limit < 36 {
                    return false;
                }
                let mut data = buf
                    .get(usize::try_from(header).unwrap_or(16)..)
                    .unwrap_or_default()
                    .to_vec();
                let remain = data.len() as u64;
                let body = self.read(limit - remain);
                let mut pos = (0, 0);
                if body.len() as u64 == limit - remain {
                    data.extend_from_slice(&body);
                    pos = parse_meta(&data);
                }
                let (at, len) = (u64::from(pos.0), u64::from(pos.1));
                if len >= 2 && len < size && size - len >= at && self.seek(at + 2) {
                    let item = self.read(len - 2);
                    if item.len() as u64 == len - 2 {
                        self.app1(&item, at + 2);
                        return true;
                    }
                }
                return false;
            }
            match offset.checked_add(box_size) {
                Some(next) => offset = next,
                None => return false,
            }
        }
        false
    }

    /// `exif_scan_thumbnail`: a JPEG thumbnail's size, from its `SOFn`.
    pub(crate) fn scan_thumbnail(&mut self) -> bool {
        let Some(data) = self.img.thumbnail.data.as_deref() else {
            return false;
        };
        let size = data
            .len()
            .min(usize::try_from(self.img.thumbnail.size).unwrap_or(usize::MAX));
        if size < 4 {
            return false;
        }
        if data[..3] != [0xFF, 0xD8, 0xFF] {
            if self.img.thumbnail.width == 0 && self.img.thumbnail.height == 0 {
                self.warn("Thumbnail is not a JPEG image");
            }
            return false;
        }
        let byte = |at: usize| data.get(at).copied().unwrap_or(0);
        let (mut pos, mut length) = (0usize, 2usize);
        loop {
            pos += length;
            if pos >= size {
                return false;
            }
            let mut c = byte(pos);
            pos += 1;
            if pos >= size || c != 0xFF {
                return false;
            }
            let mut n = 8;
            loop {
                c = byte(pos);
                pos += 1;
                if c != 0xFF || n == 0 {
                    break;
                }
                n -= 1;
                if pos + 3 >= size {
                    return false;
                }
            }
            if c == 0xFF || pos >= size {
                return false;
            }
            length = usize::from(u16::from_be_bytes([byte(pos), byte(pos + 1)]));
            if length > size || pos >= size - length {
                return false;
            }
            if marker::is_sof(u16::from(c)) {
                if length < 8 || size - 8 < pos {
                    return false;
                }
                let (height, width) = sof_size(&data[pos..]);
                self.img.thumbnail.height = u64::from(height);
                self.img.thumbnail.width = u64::from(width);
                return true;
            }
            if u16::from(c) == marker::SOS || u16::from(c) == marker::EOI {
                self.warn("Could not compute size of thumbnail");
                return false;
            }
        }
    }
}

/// The two encodings `exif.c` decodes comments from: `exif.*_unicode`
/// and `exif.*_jis`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    Unicode,
    Jis,
}

/// `exif_process_SOFn`: the height and width a frame header gives.
fn sof_size(data: &[u8]) -> (u16, u16) {
    (Order::Motorola.u16(data, 3), Order::Motorola.u16(data, 5))
}

/// `exif_ifd_make_value`: a value's bytes as a directory holds them.
/// php writes a one-byte `BYTE` or `SBYTE` from the union's integer,
/// which then holds the string's address, and an `IFD` value's numbers
/// not at all; the byte itself and zeros are written here.
fn ifd_value(entry: &crate::image::Entry, order: Order) -> Vec<u8> {
    use crate::image::{Number, Stored};
    let mut out = Vec::new();
    match &entry.value {
        Stored::Bytes(_) if entry.format == Format::Ifd => {}
        Stored::Bytes(bytes) | Stored::Text(bytes) => out.extend_from_slice(bytes),
        Stored::Numbers(numbers) => {
            for number in numbers {
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "php's `php_ifd_set16u` of a `DWORD` or `int`"
                )]
                match (entry.format, *number) {
                    (Format::UShort, Number::Unsigned(u)) => {
                        out.extend_from_slice(&order.put16(u as u16));
                    }
                    (Format::SShort, Number::Signed(i)) => {
                        out.extend_from_slice(&order.put16(i as u16));
                    }
                    (_, Number::Unsigned(u)) => out.extend_from_slice(&order.put32(u)),
                    (_, Number::Signed(i)) => out.extend_from_slice(&order.put32(i as u32)),
                    (_, Number::URational(n, d)) => {
                        out.extend_from_slice(&order.put32(n));
                        out.extend_from_slice(&order.put32(d));
                    }
                    (_, Number::SRational(n, d)) => {
                        out.extend_from_slice(&order.put32(n as u32));
                        out.extend_from_slice(&order.put32(d as u32));
                    }
                    (_, Number::Single(f)) => out.extend_from_slice(&f.to_ne_bytes()),
                    (_, Number::Double(d)) => out.extend_from_slice(&d.to_ne_bytes()),
                }
            }
        }
    }
    out.resize(entry.format.size() * entry.length, 0);
    out
}

/// `exif_isobmff_parse_box`: the header's size, the box's, its type.
fn parse_box(buf: &[u8]) -> (u64, u64, [u8; 4]) {
    let size = u64::from(Order::Motorola.u32(buf, 0));
    let kind = Order::take::<4>(buf, 4);
    if size != 1 {
        return (8, size, kind);
    }
    let wide = u64::from_be_bytes(Order::take::<8>(buf, 8));
    (16, wide, kind)
}

/// `ADVANCE(n)`: `n` bytes on, where `remain` has them.
fn advance(p: &mut usize, remain: &mut u64, n: u64) -> bool {
    if *remain < n {
        return false;
    }
    *remain -= n;
    *p += usize::try_from(n).unwrap_or(usize::MAX);
    true
}

/// `exif_isobmff_parse_meta`: the offset and size of the `Exif` item,
/// from `iinf` and `iloc`, zeros where there is none.
fn parse_meta(data: &[u8]) -> (u32, u32) {
    let end = data.len();
    let be16 = |at: usize| i64::from(Order::Motorola.u16(data, at));
    let be32 = |at: usize| Order::Motorola.u32(data, at);
    let count = |p: usize, version: u8| -> i64 {
        if version < 2 {
            be16(p - 2)
        } else {
            i64::from(i32::from_ne_bytes(be32(p - 4).to_ne_bytes()))
        }
    };
    let mut exif_id: i64 = -1;
    let mut box_at = 4;
    while box_at < end - 16 {
        let (header, size, kind) = parse_box(&data[box_at..]);
        if size < header {
            return (0, 0);
        }
        let mut p = box_at;
        let mut remain = (end - p) as u64;
        if &kind == b"iinf" {
            #[expect(clippy::cast_possible_truncation, reason = "the header is 8 or 16")]
            let skip = header as usize + 4;
            if !advance(&mut p, &mut remain, skip as u64) {
                return (0, 0);
            }
            let version = data[p - 4];
            if !advance(&mut p, &mut remain, if version < 2 { 2 } else { 4 }) {
                return (0, 0);
            }
            let items = count(p, version);
            let mut i = 0;
            while i < items && p < end - 20 {
                let (header, item_size, _) = parse_box(&data[p..]);
                if item_size < header || remain < header + 12 {
                    return (0, 0);
                }
                #[expect(clippy::cast_possible_truncation, reason = "the header is 8 or 16")]
                let at = p + header as usize;
                if data.get(at + 8..at + 12) == Some(b"Exif") {
                    exif_id = be16(at + 4);
                    break;
                }
                if !advance(&mut p, &mut remain, item_size) {
                    return (0, 0);
                }
                i += 1;
            }
            if exif_id < 0 {
                break;
            }
        } else if &kind == b"iloc" {
            #[expect(clippy::cast_possible_truncation, reason = "the header is 8 or 16")]
            let skip = header as usize + 6;
            if !advance(&mut p, &mut remain, skip as u64) {
                return (0, 0);
            }
            let version = data[p - 6];
            if !advance(&mut p, &mut remain, if version < 2 { 2 } else { 4 }) {
                return (0, 0);
            }
            let items = count(p, version);
            let mut i = 0;
            while i < items && p < end - 16 {
                if be16(p) == exif_id {
                    return (be32(p + 8), be32(p + 12));
                }
                i += 1;
                p += 16;
            }
            break;
        }
        if (end - 16 - box_at) as u64 <= size {
            break;
        }
        box_at += usize::try_from(size).unwrap_or(usize::MAX);
    }
    (0, 0)
}

/// The found sections a list of names asks for, as `exif_read_data`
/// reads `$required_sections`: the names split at commas and spaces.
pub(crate) fn needed(names: &[u8]) -> Found {
    let names = c_str(names);
    let mut needed = Found::default();
    for section in Section::ALL {
        let name = section.name().as_bytes();
        if names
            .split(|&b| b == b',' || b == b' ')
            .any(|word| word == name)
        {
            needed.add(section);
        }
    }
    needed
}
