//! `zip_algorithm_zstd.c`: ZSTD entries through libzstd's streaming
//! compressor and decompressor ([`crate::ffi::ZstdCompress`],
//! [`crate::ffi::ZstdDecompress`]), both ways.

use crate::codec::Turn;
use crate::error::{Er, Status};
use crate::ffi::{ZstdCompress, ZstdDecompress, ZstdError, ZstdRefused, levels};

/// `map_error` of a libzstd error: libzip hands it the `size_t` itself,
/// which equals none of its cases, so every error is internal.
fn map_error(_: ZstdError) -> Status {
    Status::new(Er::Internal)
}

/// `process`'s reading of a step that took `used` of `input`: the next
/// input is wanted once it has taken all of it.
fn turn(used: usize, input: &[u8]) -> Turn {
    if used == input.len() {
        Turn::NeedData
    } else {
        Turn::More
    }
}

/// `zip_algorithm_zstd_compress`.
pub(crate) struct Compress(ZstdCompress);

impl Compress {
    /// `allocate` and `start` at `level`, read as a signed level and
    /// zstd's default, 0, where libzstd has no such level.
    pub(crate) fn start(level: u32) -> Result<Compress, Status> {
        let level = level.cast_signed();
        let level = if levels().contains(&level) { level } else { 0 };
        ZstdCompress::new(level)
            .map(Compress)
            .map_err(|refused| match refused {
                ZstdRefused::Memory => Status::new(Er::Memory),
                // `ZIP_ER_ZLIB`, its detail `map_error`'s internal.
                ZstdRefused::Init(_) => Status::zlib(Er::Internal as i32),
            })
    }

    /// `process` over `input` into `out`, the frame ended once the input
    /// has: `ZSTD_compressStream` while input is left, `ZSTD_endStream`
    /// after, which is the end only once it has nothing left to flush.
    pub(crate) fn process(
        &mut self,
        input: &[u8],
        out: &mut [u8],
        finish: bool,
    ) -> Result<(Turn, usize, usize), Status> {
        if input.is_empty() && !finish {
            return Ok((Turn::NeedData, 0, 0));
        }
        let (answer, used, made) = if input.is_empty() {
            let (answer, made) = self.0.end(out);
            if matches!(answer, Ok(0)) {
                return Ok((Turn::End, 0, made));
            }
            (answer, 0, made)
        } else {
            self.0.compress(input, out)
        };
        answer.map_err(map_error)?;
        Ok((turn(used, input), used, made))
    }
}

/// `zip_algorithm_zstd_decompress`.
pub(crate) struct Decompress(ZstdDecompress);

impl Decompress {
    /// `allocate` and `start`.
    pub(crate) fn start() -> Result<Decompress, Status> {
        ZstdDecompress::new()
            .map(Decompress)
            .ok_or(Status::new(Er::Memory))
    }

    /// `process` over `input` into `out`. libzip's never answers the
    /// end: the stream ends where its input does.
    pub(crate) fn process(
        &mut self,
        input: &[u8],
        out: &mut [u8],
        finish: bool,
    ) -> Result<(Turn, usize, usize), Status> {
        if input.is_empty() && !finish {
            return Ok((Turn::NeedData, 0, 0));
        }
        let (answer, used, made) = self.0.decompress(input, out);
        answer.map_err(map_error)?;
        Ok((turn(used, input), used, made))
    }
}
