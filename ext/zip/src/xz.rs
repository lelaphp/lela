//! `zip_algorithm_xz.c`: LZMA and XZ entries through liblzma
//! ([`crate::ffi::Lzma`]), both ways. An LZMA entry is the `.lzma`
//! ("alone") format with libzip's header in place of its own: four bytes
//! of magic and the five of properties, where `.lzma` has the properties
//! and eight of uncompressed size, so libzip makes and reads the
//! `.lzma` header itself.

use crate::codec::{Method, Turn, copy_from};
use crate::error::{Er, Status};
use crate::ffi::{Lzma, LzmaCode};

/// `LZMA_PRESET_EXTREME`, which libzip asks for at every level.
const PRESET_EXTREME: u32 = 1 << 31;

/// `HEADER_BYTES_ZIP`: libzip's header of an LZMA entry.
const HEADER_BYTES_ZIP: usize = 9;

/// `HEADER_LZMA_ALONE_LENGTH`: the `.lzma` header, five bytes of
/// properties and eight of uncompressed size.
const ALONE_HEADER: usize = 13;

/// The magic libzip writes ahead of an LZMA entry's properties.
const MAGIC: [u8; 4] = [0x09, 0x14, 0x05, 0x00];

/// `map_error`.
fn map_error(code: LzmaCode) -> Status {
    Status::new(match code {
        LzmaCode::DataError | LzmaCode::UnsupportedCheck => Er::CompressedData,
        LzmaCode::MemError | LzmaCode::MemlimitError => Er::Memory,
        LzmaCode::OptionsError => Er::Inval,
        _ => Er::Internal,
    })
}

/// `process`'s reading of one `lzma_code`, which `left` input it left
/// unread.
fn turn(code: LzmaCode, left: bool) -> Result<Turn, Status> {
    match code {
        LzmaCode::Ok => Ok(Turn::More),
        LzmaCode::StreamEnd => Ok(Turn::End),
        LzmaCode::BufError if !left => Ok(Turn::NeedData),
        code => Err(map_error(code)),
    }
}

/// libzip's header of an LZMA entry on the way out (`header_state`):
/// before the encoder has made the `.lzma` one it is taken from, while it
/// is handed on, and once it has been.
enum HeaderOut {
    Incomplete,
    Output {
        bytes: [u8; HEADER_BYTES_ZIP],
        at: usize,
    },
    Done,
}

/// `zip_algorithm_xz_compress`.
pub(crate) struct Compress {
    lzma: Lzma,
    header: HeaderOut,
}

impl Compress {
    /// `allocate` and `start` at `level`: the preset is the level where
    /// it is 9 or less, else 6, extreme either way.
    pub(crate) fn start(method: Method, level: u32) -> Result<Compress, Status> {
        let preset = (if level <= 9 { level } else { 6 }) | PRESET_EXTREME;
        let alone = method == Method::Lzma;
        let lzma = Lzma::encoder(alone, preset).map_err(map_error)?;
        let header = if alone {
            HeaderOut::Incomplete
        } else {
            HeaderOut::Done
        };
        Ok(Compress { lzma, header })
    }

    /// `process` over `input` into `out`, `LZMA_FINISH` once the input
    /// has ended: what it came to, how much it took and how much it made.
    /// An LZMA entry's header is a turn of its own: the encoder's first
    /// thirteen bytes, its `.lzma` header, made into a buffer of their
    /// own, and libzip's nine handed on.
    pub(crate) fn process(
        &mut self,
        input: &[u8],
        out: &mut [u8],
        finish: bool,
    ) -> Result<(Turn, usize, usize), Status> {
        let mut used = 0;
        if matches!(self.header, HeaderOut::Incomplete) {
            let mut header = [0; MAGIC.len() + ALONE_HEADER];
            header[..MAGIC.len()].copy_from_slice(&MAGIC);
            let (code, took, made) = self.lzma.code(input, &mut header[MAGIC.len()..], false);
            // "assume that the whole header will be provided with the
            // first call to lzma_code"; libzip sets no error, and
            // `compress_read` reads that as internal.
            if code != LzmaCode::Ok || made != ALONE_HEADER {
                return Err(Status::new(Er::Internal));
            }
            used = took;
            let mut bytes = [0; HEADER_BYTES_ZIP];
            bytes.copy_from_slice(&header[..HEADER_BYTES_ZIP]);
            self.header = HeaderOut::Output { bytes, at: 0 };
        }
        if let HeaderOut::Output { bytes, at } = &mut self.header {
            let n = copy_from(bytes, *at, out);
            *at += n;
            if *at == HEADER_BYTES_ZIP {
                self.header = HeaderOut::Done;
            }
            return Ok((Turn::More, used, n));
        }
        let (code, took, made) = self.lzma.code(input, out, finish);
        Ok((turn(code, took < input.len())?, took, made))
    }
}

/// libzip's header of an LZMA entry on the way in (`header_state`): the
/// bytes of it read so far, until the decoder has been fed them.
enum HeaderIn {
    Incomplete {
        bytes: [u8; HEADER_BYTES_ZIP],
        have: usize,
    },
    Done,
}

/// `zip_algorithm_xz_decompress`.
pub(crate) struct Decompress {
    lzma: Lzma,
    header: HeaderIn,
    /// `uncompresssed_size`, which the `.lzma` header states: the
    /// entry's, or unknown, `u64::MAX`, where an end marker ends it.
    size: u64,
}

impl Decompress {
    /// `allocate` and `start`, `size` the uncompressed size the `.lzma`
    /// header states.
    pub(crate) fn start(method: Method, size: u64) -> Result<Decompress, Status> {
        let alone = method == Method::Lzma;
        let lzma = Lzma::decoder(alone).map_err(map_error)?;
        let header = if alone {
            HeaderIn::Incomplete {
                bytes: [0; HEADER_BYTES_ZIP],
                have: 0,
            }
        } else {
            HeaderIn::Done
        };
        Ok(Decompress { lzma, header, size })
    }

    /// `input`: libzip's header of an LZMA entry, taken from the front of
    /// `data` until it is whole, then fed to the decoder as the `.lzma`
    /// header: how much of `data` it took, or the error that leaves the
    /// rest unread, which `compress_read` keeps and reads on past.
    pub(crate) fn input(&mut self, data: &[u8]) -> Result<usize, Status> {
        let HeaderIn::Incomplete { bytes, have } = &mut self.header else {
            return Ok(0);
        };
        let got = (HEADER_BYTES_ZIP - *have).min(data.len());
        bytes[*have..*have + got].copy_from_slice(&data[..got]);
        *have += got;
        if *have < HEADER_BYTES_ZIP {
            return Ok(got);
        }
        if bytes[2..4] != MAGIC[2..] {
            return Err(Status::new(Er::CompressedData));
        }
        let mut alone = [0; ALONE_HEADER];
        alone[..5].copy_from_slice(&bytes[4..]);
        alone[5..].copy_from_slice(&self.size.to_le_bytes());
        // The header alone initialises the decoder and makes nothing.
        let (code, _, made) = self.lzma.code(&alone, &mut [0], false);
        if code != LzmaCode::Ok || made > 0 {
            return Err(Status::new(Er::CompressedData));
        }
        self.header = HeaderIn::Done;
        Ok(got)
    }

    /// `process` over `input` into `out`, `LZMA_FINISH` once the input
    /// has ended.
    pub(crate) fn process(
        &mut self,
        input: &[u8],
        out: &mut [u8],
        finish: bool,
    ) -> Result<(Turn, usize, usize), Status> {
        let (code, took, made) = self.lzma.code(input, out, finish);
        Ok((turn(code, took < input.len())?, took, made))
    }
}
