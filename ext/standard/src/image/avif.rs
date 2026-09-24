//! The parts of libavifinfo that `Z/ext/standard/image.c` calls,
//! `Z/ext/standard/libavifinfo/avifinfo.c`: `AvifInfoIdentifyStream` for
//! the type test and `AvifInfoGetFeaturesStream` for the size.
//!
//! php wraps its stream in `php_avif_stream_read` / `php_avif_stream_skip`
//! (`Z/ext/standard/image.c:1154`): a short read fails that read and every
//! later one, and a skip is a forward seek. [`AvifStream`] does the same.

use super::Stream;

/// `AVIFINFO_MAX_SIZE`, `Z/ext/standard/libavifinfo/avifinfo.c:36`.
const MAX_SIZE: u32 = u32::MAX;
/// `AVIFINFO_MAX_NUM_BOXES`, avifinfo.c:38.
const MAX_NUM_BOXES: u32 = 4096;
// `AVIFINFO_MAX_VALUE` (255, avifinfo.c:40) bounds every id and index kept;
// `u8::try_from` is that test here.
/// `AVIFINFO_MAX_TILES`, avifinfo.c:42.
const MAX_TILES: usize = 16;
/// `AVIFINFO_MAX_PROPS`, avifinfo.c:43.
const MAX_PROPS: usize = 32;
/// `AVIFINFO_MAX_FEATURES`, avifinfo.c:44.
const MAX_FEATURES: usize = 8;
/// `AVIFINFO_MAX_NUM_READ_BYTES`, `Z/ext/standard/libavifinfo/avifinfo.h:92`.
const MAX_NUM_READ_BYTES: usize = 64;

/// `AvifInfoInternalStatus`, avifinfo.c:19.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    /// `kFound`.
    Found,
    /// `kNotFound`.
    NotFound,
    /// `kTruncated`.
    Truncated,
    /// `kAborted`.
    Aborted,
    /// `kInvalid`.
    Invalid,
}

use Status::{Aborted, Found, Invalid, NotFound, Truncated};

/// `AVIFINFO_CHECK`, avifinfo.c:83.
macro_rules! check {
    ($cond:expr, $status:expr) => {
        if !($cond) {
            return $status;
        }
    };
}

/// `AVIFINFO_CHECK_FOUND`, avifinfo.c:92.
macro_rules! check_found {
    ($e:expr) => {
        let s = $e;
        if s != Found {
            return s;
        }
    };
}

/// `AVIFINFO_CHECK_NOT_FOUND`, avifinfo.c:94.
macro_rules! check_not_found {
    ($e:expr) => {
        let s = $e;
        if s != NotFound {
            return s;
        }
    };
}

/// `AvifInfoInternalRead` as a statement: the bytes, or return its status.
macro_rules! read {
    ($stream:expr, $n:expr) => {
        match $stream.read($n) {
            Some(d) => d,
            None => return Truncated,
        }
    };
}

/// `struct php_avif_stream` with the `AvifInfoInternalStream` around it,
/// `Z/ext/standard/image.c:1149` and avifinfo.c:106.
struct AvifStream<'s, 'a> {
    /// `None` once a read came back short ("fail further calls").
    stream: Option<&'s mut Stream<'a>>,
    /// The one buffer every read lands in.
    buffer: [u8; MAX_NUM_READ_BYTES],
}

impl<'s, 'a> AvifStream<'s, 'a> {
    /// Wraps php's stream at its current position.
    fn new(stream: &'s mut Stream<'a>) -> Self {
        AvifStream {
            stream: Some(stream),
            buffer: [0; MAX_NUM_READ_BYTES],
        }
    }

    /// `php_avif_stream_read`, `Z/ext/standard/image.c:1154`, behind
    /// `AvifInfoInternalRead`, avifinfo.c:116. `n` is 1 to 64.
    fn read(&mut self, n: u32) -> Option<[u8; MAX_NUM_READ_BYTES]> {
        let stream = self.stream.as_deref_mut()?;
        let n = wide(n).min(MAX_NUM_READ_BYTES);
        let dst = self.buffer.get_mut(..n)?;
        if stream.read(dst) != n {
            self.stream = None;
            return None;
        }
        Some(self.buffer)
    }

    /// `php_avif_stream_skip`, `Z/ext/standard/image.c:1167`, behind
    /// `AvifInfoInternalSkip`, avifinfo.c:125. It cannot fail the parse.
    fn skip(&mut self, n: u32) -> Status {
        if n > 0
            && let Some(stream) = self.stream.as_deref_mut()
            && !stream.seek_cur(i64::from(n))
        {
            self.stream = None;
        }
        Found
    }
}

/// A `u32` count as a `usize`, for the lengths `AvifInfoInternalReadBigEndian`
/// (avifinfo.c:49) takes.
fn wide(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// `AvifInfoInternalReadBigEndian`, avifinfo.c:49, over `n` bytes at `at`.
fn big_endian(data: &[u8], at: usize, n: usize) -> u32 {
    data.iter()
        .skip(at)
        .take(n)
        .fold(0u32, |v, &b| (v << 8) | u32::from(b))
}

/// `AvifInfoFeatures`, `Z/ext/standard/libavifinfo/avifinfo.h:37`, less the
/// primary item location fields, which php never reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Features {
    /// In pixels, crop and rotation ignored.
    pub(super) width: u32,
    /// In pixels.
    pub(super) height: u32,
    /// Bits per channel.
    pub(super) bit_depth: u32,
    /// Channels, alpha included.
    pub(super) num_channels: u32,
    /// Whether a gain map was found.
    has_gainmap: bool,
    /// Its item id.
    gainmap_item_id: u8,
}

/// `AvifInfoInternalTile`, avifinfo.c:147.
#[allow(clippy::struct_field_names)] // libavifinfo's names
#[derive(Clone, Copy, Debug)]
struct Tile {
    tile_item_id: u8,
    parent_item_id: u8,
    dimg_idx: u8,
}

/// `AvifInfoInternalProp`, avifinfo.c:153.
#[derive(Clone, Copy, Debug)]
struct Prop {
    property_index: u8,
    item_id: u8,
}

/// `AvifInfoInternalDimProp`, avifinfo.c:158.
#[derive(Clone, Copy, Debug)]
struct DimProp {
    property_index: u8,
    width: u32,
    height: u32,
}

/// `AvifInfoInternalChanProp`, avifinfo.c:163.
#[derive(Clone, Copy, Debug)]
struct ChanProp {
    property_index: u8,
    bit_depth: u8,
    num_channels: u8,
}

/// `AvifInfoInternalFeatures`, avifinfo.c:168. The fixed arrays and their
/// counts are vectors capped at the same sizes.
#[allow(clippy::struct_excessive_bools)] // libavifinfo's fields
#[derive(Debug, Default)]
struct Internal {
    has_primary_item: bool,
    has_alpha: bool,
    gainmap_property_index: u8,
    primary_item_id: u8,
    primary_item_features: Features,
    data_was_skipped: bool,
    tone_mapped_item_id: u8,
    iinf_parsed: bool,
    iref_parsed: bool,
    tiles: Vec<Tile>,
    props: Vec<Prop>,
    dim_props: Vec<DimProp>,
    chan_props: Vec<ChanProp>,
}

/// `AvifInfoInternalGetItemFeatures`, avifinfo.c:191.
fn get_item_features(f: &mut Internal, target_item_id: u32, tile_depth: u32) -> Status {
    for p in 0..f.props.len() {
        let Some(prop) = f.props.get(p).copied() else {
            break;
        };
        if u32::from(prop.item_id) != target_item_id {
            continue;
        }
        let property_index = prop.property_index;

        if target_item_id == u32::from(f.primary_item_id)
            && (f.primary_item_features.width == 0 || f.primary_item_features.height == 0)
            && let Some(d) = f
                .dim_props
                .iter()
                .find(|d| d.property_index == property_index)
        {
            f.primary_item_features.width = d.width;
            f.primary_item_features.height = d.height;
            if f.primary_item_features.bit_depth != 0 && f.primary_item_features.num_channels != 0 {
                return Found;
            }
        }
        if (f.primary_item_features.bit_depth == 0 || f.primary_item_features.num_channels == 0)
            && let Some(c) = f
                .chan_props
                .iter()
                .find(|c| c.property_index == property_index)
        {
            f.primary_item_features.bit_depth = u32::from(c.bit_depth);
            f.primary_item_features.num_channels = u32::from(c.num_channels);
            if f.primary_item_features.width != 0 && f.primary_item_features.height != 0 {
                return Found;
            }
        }
    }

    let mut tile = 0;
    while tile < f.tiles.len() && tile_depth < 3 {
        if let Some(t) = f.tiles.get(tile).copied()
            && u32::from(t.parent_item_id) == target_item_id
        {
            check_not_found!(get_item_features(
                f,
                u32::from(t.tile_item_id),
                tile_depth + 1
            ));
        }
        tile += 1;
    }
    NotFound
}

/// `AvifInfoInternalGetPrimaryItemFeatures`, avifinfo.c:240.
fn get_primary_item_features(f: &mut Internal) -> Status {
    check!(f.has_primary_item, NotFound);
    check!(
        !f.dim_props.is_empty() && !f.chan_props.is_empty(),
        NotFound
    );

    // HEIF scheme: the gain map is a hidden input of a derived item.
    if f.tone_mapped_item_id != 0
        && let Some(t) = f
            .tiles
            .iter()
            .find(|t| t.parent_item_id == f.tone_mapped_item_id && t.dimg_idx == 1)
    {
        f.primary_item_features.has_gainmap = true;
        f.primary_item_features.gainmap_item_id = t.tile_item_id;
    }
    // Adobe scheme: the gain map is an auxiliary item.
    if !f.primary_item_features.has_gainmap
        && f.gainmap_property_index > 0
        && let Some(p) = f
            .props
            .iter()
            .find(|p| p.property_index == f.gainmap_property_index)
    {
        f.primary_item_features.has_gainmap = true;
        f.primary_item_features.gainmap_item_id = p.item_id;
    }
    if !f.primary_item_features.has_gainmap
        && (!f.iinf_parsed || (f.tone_mapped_item_id != 0 && !f.iref_parsed))
    {
        return NotFound;
    }

    let primary = u32::from(f.primary_item_id);
    check_found!(get_item_features(f, primary, 0));

    if f.has_alpha {
        f.primary_item_features.num_channels = f.primary_item_features.num_channels.wrapping_add(1);
    }
    Found
}

/// `AvifInfoInternalBox`, avifinfo.c:287.
#[derive(Clone, Copy, Debug)]
struct BoxHeader {
    size: u32,
    kind: [u8; 4],
    version: u32,
    flags: u32,
    content_size: u32,
}

/// `AvifInfoInternalParseBox`, avifinfo.c:298: a box header at the cursor.
fn parse_box(
    nesting_level: u32,
    stream: &mut AvifStream<'_, '_>,
    num_remaining_bytes: u32,
    num_parsed_boxes: &mut u32,
) -> Result<BoxHeader, Status> {
    let mut box_header_size: u32 = 8;
    if box_header_size > num_remaining_bytes {
        return Err(Invalid);
    }
    let data = stream.read(8).ok_or(Truncated)?;
    let mut size = big_endian(&data, 0, 4);
    let mut kind = [data[4], data[5], data[6], data[7]];
    if size == 1 {
        box_header_size += 8;
        if box_header_size > num_remaining_bytes {
            return Err(Invalid);
        }
        let data = stream.read(8).ok_or(Truncated)?;
        // Past 4 GB the parse stops.
        if big_endian(&data, 0, 4) != 0 {
            return Err(Aborted);
        }
        size = big_endian(&data, 4, 4);
    } else if size == 0 {
        // A box running to the end is top-level only.
        if nesting_level != 0 {
            return Err(Invalid);
        }
        size = num_remaining_bytes;
    }
    if size < box_header_size || size > num_remaining_bytes {
        return Err(Invalid);
    }

    let has_fullbox_header = matches!(
        &kind,
        b"meta" | b"pitm" | b"ipma" | b"ispe" | b"pixi" | b"iref" | b"auxC" | b"iinf" | b"infe"
    );
    if has_fullbox_header {
        box_header_size += 4;
    }
    if size < box_header_size {
        return Err(Invalid);
    }
    let content_size = size - box_header_size;
    // Top-level "ftyp" boxes are not counted, so a stream read from its start
    // and one read after the type test stop alike.
    if nesting_level != 0 || &kind != b"ftyp" {
        *num_parsed_boxes += 1;
        if *num_parsed_boxes >= MAX_NUM_BOXES {
            return Err(Aborted);
        }
    }

    let mut version = 0;
    let mut flags = 0;
    if has_fullbox_header {
        let data = stream.read(4).ok_or(Truncated)?;
        version = big_endian(&data, 0, 1);
        flags = big_endian(&data, 1, 3);
        let is_parsable = match &kind {
            b"meta" | b"ispe" | b"pixi" | b"auxC" => version == 0,
            b"pitm" | b"ipma" | b"iref" | b"iinf" => version <= 1,
            b"infe" => (2..=3).contains(&version),
            _ => false,
        };
        // An unparsable box is skipped rather than failing the file.
        if !is_parsable {
            kind = *b"skip";
        }
    }
    Ok(BoxHeader {
        size,
        kind,
        version,
        flags,
        content_size,
    })
}

/// `parse_box` inside a function that returns a [`Status`].
macro_rules! parse_box {
    ($level:expr, $stream:expr, $remaining:expr, $boxes:expr) => {
        match parse_box($level, $stream, $remaining, $boxes) {
            Ok(b) => b,
            Err(s) => return s,
        }
    };
}

/// `ParseIpco`, avifinfo.c:387: the item properties.
fn parse_ipco(
    nesting_level: u32,
    stream: &mut AvifStream<'_, '_>,
    mut num_remaining_bytes: u32,
    num_parsed_boxes: &mut u32,
    f: &mut Internal,
) -> Status {
    let mut box_index: u32 = 1;
    loop {
        let b = parse_box!(nesting_level, stream, num_remaining_bytes, num_parsed_boxes);
        let index = u8::try_from(box_index).ok();

        if &b.kind == b"ispe" {
            check!(b.content_size >= 8, Invalid);
            let data = read!(stream, 8);
            let width = big_endian(&data, 0, 4);
            let height = big_endian(&data, 4, 4);
            check!(width != 0 && height != 0, Invalid);
            match index {
                Some(property_index) if f.dim_props.len() < MAX_FEATURES => {
                    f.dim_props.push(DimProp {
                        property_index,
                        width,
                        height,
                    });
                }
                _ => f.data_was_skipped = true,
            }
            check_found!(stream.skip(b.content_size.wrapping_sub(8)));
        } else if &b.kind == b"pixi" {
            check_found!(ipco_pixi(stream, &b, index, f));
        } else if &b.kind == b"av1C" {
            check!(b.content_size >= 3, Invalid);
            let data = read!(stream, 3);
            let high_bitdepth = data[2] & 0x40 != 0;
            let twelve_bit = data[2] & 0x20 != 0;
            let monochrome = data[2] & 0x10 != 0;
            if twelve_bit {
                check!(high_bitdepth, Invalid);
            }
            match index {
                Some(property_index) if f.chan_props.len() < MAX_FEATURES => {
                    let bit_depth = match (high_bitdepth, twelve_bit) {
                        (true, true) => 12,
                        (true, false) => 10,
                        _ => 8,
                    };
                    f.chan_props.push(ChanProp {
                        property_index,
                        bit_depth,
                        num_channels: if monochrome { 1 } else { 3 },
                    });
                }
                _ => f.data_was_skipped = true,
            }
            check_found!(stream.skip(b.content_size.wrapping_sub(3)));
        } else if &b.kind == b"auxC" {
            check_found!(ipco_auxc(stream, &b, index, f));
        } else {
            check_found!(stream.skip(b.content_size));
        }
        box_index = box_index.wrapping_add(1);
        num_remaining_bytes = num_remaining_bytes.wrapping_sub(b.size);
        if num_remaining_bytes == 0 {
            break;
        }
    }
    NotFound
}

/// The "pixi" branch of `ParseIpco`, avifinfo.c:416: bits per channel and
/// the channel count.
fn ipco_pixi(
    stream: &mut AvifStream<'_, '_>,
    b: &BoxHeader,
    index: Option<u8>,
    f: &mut Internal,
) -> Status {
    check!(b.content_size >= 1, Invalid);
    let data = read!(stream, 1);
    let num_channels = big_endian(&data, 0, 1);
    check!(num_channels >= 1, Invalid);
    check!(b.content_size > num_channels, Invalid);
    let data = read!(stream, 1);
    let bit_depth = big_endian(&data, 0, 1);
    check!(bit_depth >= 1, Invalid);
    for i in 1..num_channels {
        let data = read!(stream, 1);
        // Every channel has the same depth.
        check!(big_endian(&data, 0, 1) == bit_depth, Invalid);
        check!(i <= 32, Aborted);
    }
    match (
        index,
        u8::try_from(bit_depth).ok(),
        u8::try_from(num_channels).ok(),
    ) {
        (Some(property_index), Some(bit_depth), Some(num_channels))
            if f.chan_props.len() < MAX_FEATURES =>
        {
            f.chan_props.push(ChanProp {
                property_index,
                bit_depth,
                num_channels,
            });
        }
        _ => f.data_was_skipped = true,
    }
    stream.skip(b.content_size.wrapping_sub(1 + num_channels))
}

/// The "auxC" branch of `ParseIpco`, avifinfo.c:474: an alpha plane or a
/// gain map.
fn ipco_auxc(
    stream: &mut AvifStream<'_, '_>,
    b: &BoxHeader,
    index: Option<u8>,
    f: &mut Internal,
) -> Status {
    // With their terminating NUL, which strcmp compares too.
    const ALPHA: &[u8; 44] = b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha\0";
    const GAINMAP: &[u8; 29] = b"urn:com:photo:aux:hdrgainmap\0";
    let mut num_read_bytes: u32 = 0;
    if b.content_size >= 29 {
        let data = read!(stream, 29);
        num_read_bytes = 29;
        if data.get(..29) == Some(&GAINMAP[..]) {
            match index {
                Some(i) => f.gainmap_property_index = i,
                None => f.data_was_skipped = true,
            }
        } else if b.content_size >= 44 && data.get(..29) == ALPHA.get(..29) {
            let data2 = read!(stream, 15);
            num_read_bytes = 44;
            if data2.get(..15) == ALPHA.get(29..) {
                f.has_alpha = true;
            }
        }
    }
    stream.skip(b.content_size.wrapping_sub(num_read_bytes))
}

/// `ParseIprp`, avifinfo.c:527: "ipco" and the "ipma" associations.
fn parse_iprp(
    nesting_level: u32,
    stream: &mut AvifStream<'_, '_>,
    mut num_remaining_bytes: u32,
    num_parsed_boxes: &mut u32,
    f: &mut Internal,
) -> Status {
    loop {
        let b = parse_box!(nesting_level, stream, num_remaining_bytes, num_parsed_boxes);

        if &b.kind == b"ipco" {
            check_not_found!(parse_ipco(
                nesting_level + 1,
                stream,
                b.content_size,
                num_parsed_boxes,
                f
            ));
        } else if &b.kind == b"ipma" {
            let mut num_read_bytes: u32 = 4;
            check!(b.content_size >= num_read_bytes, Invalid);
            let data = read!(stream, 4);
            let entry_count = big_endian(&data, 0, 4);
            let id_num_bytes: u32 = if b.version < 1 { 2 } else { 4 };
            let index_num_bytes: u32 = if b.flags & 1 != 0 { 2 } else { 1 };
            let essential_bit_mask: u32 = if b.flags & 1 != 0 { 0x8000 } else { 0x80 };

            let mut entry: u32 = 0;
            while entry < entry_count {
                if wide(entry) >= MAX_PROPS || f.props.len() >= MAX_PROPS {
                    f.data_was_skipped = true;
                    break;
                }
                num_read_bytes = num_read_bytes.wrapping_add(id_num_bytes + 1);
                check!(b.content_size >= num_read_bytes, Invalid);
                let data = read!(stream, id_num_bytes + 1);
                let id_len = wide(id_num_bytes);
                let item_id = big_endian(&data, 0, id_len);
                let association_count = big_endian(&data, id_len, 1);

                let mut property: u32 = 0;
                while property < association_count {
                    if wide(property) >= MAX_PROPS || f.props.len() >= MAX_PROPS {
                        f.data_was_skipped = true;
                        break;
                    }
                    num_read_bytes = num_read_bytes.wrapping_add(index_num_bytes);
                    check!(b.content_size >= num_read_bytes, Invalid);
                    let data = read!(stream, index_num_bytes);
                    let value = big_endian(&data, 0, wide(index_num_bytes));
                    let property_index = value & !essential_bit_mask;
                    match (u8::try_from(property_index), u8::try_from(item_id)) {
                        (Ok(property_index), Ok(item_id)) => {
                            f.props.push(Prop {
                                property_index,
                                item_id,
                            });
                        }
                        _ => f.data_was_skipped = true,
                    }
                    property += 1;
                }
                // Do not read past what was skipped.
                if property < association_count {
                    break;
                }
                entry += 1;
            }

            // Everything may be known now.
            check_not_found!(get_primary_item_features(f));

            check_found!(stream.skip(b.content_size.wrapping_sub(num_read_bytes)));
        } else {
            check_found!(stream.skip(b.content_size));
        }
        num_remaining_bytes = num_remaining_bytes.wrapping_sub(b.size);
        if num_remaining_bytes == 0 {
            break;
        }
    }
    NotFound
}

/// `ParseIref`, avifinfo.c:616: the "dimg" links from tiles to their parent.
fn parse_iref(
    nesting_level: u32,
    stream: &mut AvifStream<'_, '_>,
    mut num_remaining_bytes: u32,
    num_parsed_boxes: &mut u32,
    f: &mut Internal,
) -> Status {
    f.iref_parsed = true;

    while num_remaining_bytes > 0 {
        let b = parse_box!(nesting_level, stream, num_remaining_bytes, num_parsed_boxes);

        if &b.kind == b"dimg" {
            let num_bytes_per_id: u32 = if b.version == 0 { 2 } else { 4 };
            let id_len = wide(num_bytes_per_id);
            let mut num_read_bytes = num_bytes_per_id + 2;
            check!(b.content_size >= num_read_bytes, Invalid);
            let data = read!(stream, num_bytes_per_id + 2);
            let from_item_id = big_endian(&data, 0, id_len);
            let reference_count = big_endian(&data, id_len, 2);

            for i in 0..reference_count {
                if wide(i) >= MAX_TILES {
                    f.data_was_skipped = true;
                    break;
                }
                num_read_bytes = num_read_bytes.wrapping_add(num_bytes_per_id);
                check!(b.content_size >= num_read_bytes, Invalid);
                let data = read!(stream, num_bytes_per_id);
                let to_item_id = big_endian(&data, 0, id_len);
                match (
                    u8::try_from(from_item_id),
                    u8::try_from(to_item_id),
                    u8::try_from(i),
                ) {
                    (Ok(parent_item_id), Ok(tile_item_id), Ok(dimg_idx))
                        if f.tiles.len() < MAX_TILES =>
                    {
                        f.tiles.push(Tile {
                            tile_item_id,
                            parent_item_id,
                            dimg_idx,
                        });
                    }
                    _ => f.data_was_skipped = true,
                }
            }

            // Everything may be known now.
            check_not_found!(get_primary_item_features(f));

            check_found!(stream.skip(b.content_size.wrapping_sub(num_read_bytes)));
        } else {
            check_found!(stream.skip(b.content_size));
        }
        num_remaining_bytes = num_remaining_bytes.wrapping_sub(b.size);
    }
    NotFound
}

/// `ParseIinf`, avifinfo.c:682: the item types, for a "tmap" gain map.
fn parse_iinf(
    nesting_level: u32,
    stream: &mut AvifStream<'_, '_>,
    mut num_remaining_bytes: u32,
    box_version: u32,
    num_parsed_boxes: &mut u32,
    f: &mut Internal,
) -> Status {
    f.iinf_parsed = true;

    let num_bytes_per_entry_count: u32 = if box_version == 0 { 2 } else { 4 };
    check!(num_bytes_per_entry_count <= num_remaining_bytes, Invalid);
    let data = read!(stream, num_bytes_per_entry_count);
    num_remaining_bytes -= num_bytes_per_entry_count;
    let entry_count = big_endian(&data, 0, wide(num_bytes_per_entry_count));

    for _ in 0..entry_count {
        let b = parse_box!(nesting_level, stream, num_remaining_bytes, num_parsed_boxes);

        if &b.kind == b"infe" {
            let num_bytes_per_id: u32 = if b.version == 2 { 2 } else { 4 };
            // item_ID, item_protection_index and item_type.
            check!(num_bytes_per_id + 2 + 4 <= b.content_size, Invalid);
            let data = read!(stream, num_bytes_per_id);
            let item_id = big_endian(&data, 0, wide(num_bytes_per_id));

            check_found!(stream.skip(2));

            let item_type = read!(stream, 4);
            if item_type.get(..4) == Some(&b"tmap"[..]) {
                match u8::try_from(item_id) {
                    Ok(id) => f.tone_mapped_item_id = id,
                    Err(_) => f.data_was_skipped = true,
                }
            }

            check_found!(stream.skip(b.content_size.wrapping_sub(num_bytes_per_id + 2 + 4)));
        } else {
            check_found!(stream.skip(b.content_size));
        }

        num_remaining_bytes = num_remaining_bytes.wrapping_sub(b.size);
        // An entry count larger than the box is ignored.
        if num_remaining_bytes == 0 {
            break;
        }
    }
    NotFound
}

/// `ParseMeta`, avifinfo.c:745: "pitm" names the primary item; "iprp",
/// "iref" and "iinf" describe it.
fn parse_meta(
    nesting_level: u32,
    stream: &mut AvifStream<'_, '_>,
    mut num_remaining_bytes: u32,
    num_parsed_boxes: &mut u32,
    f: &mut Internal,
) -> Status {
    loop {
        let b = parse_box!(nesting_level, stream, num_remaining_bytes, num_parsed_boxes);
        if &b.kind == b"pitm" {
            let num_bytes_per_id: u32 = if b.version == 0 { 2 } else { 4 };
            // Compared with the rest of "meta", not with this box.
            check!(num_bytes_per_id <= num_remaining_bytes, Invalid);
            let data = read!(stream, num_bytes_per_id);
            let primary_item_id = big_endian(&data, 0, wide(num_bytes_per_id));
            let Ok(primary_item_id) = u8::try_from(primary_item_id) else {
                return Aborted;
            };
            f.has_primary_item = true;
            f.primary_item_id = primary_item_id;
            // Unguarded in libavifinfo: a short "pitm" wraps to a huge skip.
            check_found!(stream.skip(b.content_size.wrapping_sub(num_bytes_per_id)));
        } else if &b.kind == b"iprp" {
            check_not_found!(parse_iprp(
                nesting_level + 1,
                stream,
                b.content_size,
                num_parsed_boxes,
                f
            ));
        } else if &b.kind == b"iref" {
            check_not_found!(parse_iref(
                nesting_level + 1,
                stream,
                b.content_size,
                num_parsed_boxes,
                f
            ));
        } else if &b.kind == b"iinf" {
            check_not_found!(parse_iinf(
                nesting_level + 1,
                stream,
                b.content_size,
                b.version,
                num_parsed_boxes,
                f
            ));
        } else {
            check_found!(stream.skip(b.content_size));
        }
        num_remaining_bytes = num_remaining_bytes.wrapping_sub(b.size);
        if num_remaining_bytes == 0 {
            break;
        }
    }
    // There is at most one "meta".
    if f.data_was_skipped { Aborted } else { Invalid }
}

/// `ParseFtyp`, avifinfo.c:796: the "ftyp" box must list "avif" or "avis".
fn parse_ftyp(stream: &mut AvifStream<'_, '_>) -> Status {
    let mut num_parsed_boxes: u32 = 0;
    let b = parse_box!(0, stream, MAX_SIZE, &mut num_parsed_boxes);
    check!(&b.kind == b"ftyp", Invalid);
    // major_brand and minor_version.
    check!(b.content_size >= 8, Invalid);
    let mut i: u32 = 0;
    while i.wrapping_add(4) <= b.content_size {
        let data = read!(stream, 4);
        if i != 4 {
            let brand = data.get(..4);
            if brand == Some(&b"avif"[..]) || brand == Some(&b"avis"[..]) {
                check_found!(stream.skip(b.content_size.wrapping_sub(i + 4)));
                return Found;
            }
            check!(i <= 32 * 4, Aborted);
        }
        i = i.wrapping_add(4);
    }
    Invalid
}

/// `ParseFile`, avifinfo.c:820: top-level boxes up to "meta".
fn parse_file(
    stream: &mut AvifStream<'_, '_>,
    num_parsed_boxes: &mut u32,
    f: &mut Internal,
) -> Status {
    loop {
        let b = parse_box!(0, stream, MAX_SIZE, num_parsed_boxes);
        if &b.kind == b"meta" {
            return parse_meta(1, stream, b.content_size, num_parsed_boxes, f);
        }
        check_found!(stream.skip(b.content_size));
    }
}

/// `php_is_image_avif`, `Z/ext/standard/image.c:1211`, through
/// `AvifInfoIdentifyStream`, avifinfo.c:893: whether the first box is an
/// "ftyp" naming an AVIF brand. The cursor is left after that box.
pub(super) fn identify(s: &mut Stream<'_>) -> bool {
    let mut stream = AvifStream::new(s);
    parse_ftyp(&mut stream) == Found
}

/// `AvifInfoGetFeaturesStream`, avifinfo.c:905, as `php_handle_avif`
/// (`Z/ext/standard/image.c:1188`) calls it from the cursor's position.
pub(super) fn features(s: &mut Stream<'_>) -> Option<Features> {
    let mut stream = AvifStream::new(s);
    let mut num_parsed_boxes: u32 = 0;
    let mut f = Internal::default();
    (parse_file(&mut stream, &mut num_parsed_boxes, &mut f) == Found)
        .then_some(f.primary_item_features)
}
