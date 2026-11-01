//! `add_data`'s `src_final` (`zip_close.c`): an entry's source under the
//! CRC layer, the compression layer and the encryption layer the close
//! lays over it, read as `copy_source` reads it, a buffer at a time, each
//! layer reading the one below as `zip_source_read` does, until its
//! buffer is full or the layer below has ended.

use crate::archive::{Archive, Input};
use crate::codec::{Attributes, Compressor, Sealer};
use crate::error::Status;

/// The source, read through `add_data`'s CRC layer, which hashes and
/// counts what it passes on (`zip_source_crc_create(src, 0, ...)`).
struct Lower {
    input: Input,
    hasher: crc32fast::Hasher,
    read: u64,
    eof: bool,
}

/// `src_final`, the source and the layers over it, and how much it has
/// given.
pub(crate) struct Pipe {
    lower: Lower,
    compressor: Option<Compressor>,
    sealer: Option<Sealer>,
    /// `zip_source_read`'s end of the compression layer, and of the top.
    compressed_eof: bool,
    eof: bool,
    written: u64,
    /// `data_length`: the source's stated size, where it states one.
    size: Option<u64>,
    /// What the source gives below the layers.
    below: Attributes,
}

/// `zip_source_read` over one layer: `read` until `out` is full or the
/// layer gives nothing, which ends it for good (`src->eof`).
fn fill(
    eof: &mut bool,
    out: &mut [u8],
    read: &mut dyn FnMut(&mut [u8]) -> Result<usize, Status>,
) -> Result<usize, Status> {
    let mut got = 0;
    while !*eof && got < out.len() {
        match read(&mut out[got..])? {
            0 => *eof = true,
            n => got += n,
        }
    }
    Ok(got)
}

impl Pipe {
    pub(crate) fn new(
        input: Input,
        size: Option<u64>,
        below: Attributes,
        compressor: Option<Compressor>,
        sealer: Option<Sealer>,
    ) -> Pipe {
        Pipe {
            lower: Lower {
                input,
                hasher: crc32fast::Hasher::new(),
                read: 0,
                eof: false,
            },
            compressor,
            sealer,
            compressed_eof: false,
            eof: false,
            written: 0,
            size,
            below,
        }
    }

    /// One `zip_source_read` of `src_final` into `out`, the archive
    /// reached for the source below.
    pub(crate) fn read(&mut self, za: &mut Archive, out: &mut [u8]) -> Result<usize, Status> {
        let Pipe {
            lower,
            compressor,
            sealer,
            compressed_eof,
            eof,
            ..
        } = self;
        let mut source = |buf: &mut [u8]| -> Result<usize, Status> {
            let Lower {
                input,
                hasher,
                read,
                eof,
            } = &mut *lower;
            let n = fill(eof, buf, &mut |b| za.read_input(input, b))?;
            hasher.update(&buf[..n]);
            *read += n as u64;
            Ok(n)
        };
        let mut compressed = |buf: &mut [u8]| -> Result<usize, Status> {
            match compressor {
                Some(c) => fill(compressed_eof, buf, &mut |b| c.read(b, &mut source)),
                None => source(buf),
            }
        };
        let n = fill(eof, out, &mut |buf| match sealer {
            Some(s) => s.read(buf, &mut compressed),
            None => compressed(buf),
        })?;
        self.written += n as u64;
        Ok(n)
    }

    /// `copy_source`'s progress at a full buffer: `zip_source_tell` of
    /// the source, asked of the archive then, over its stated size; `None`
    /// where it states none, or 0 (`data_length > 0`).
    #[expect(
        clippy::cast_precision_loss,
        reason = "copy_source's progress as libzip's doubles"
    )]
    pub(crate) fn share(&self, za: &mut Archive) -> Option<f64> {
        let size = self.size.filter(|&s| s > 0)?;
        Some(za.tell(&self.lower.input) as f64 / size as f64)
    }

    /// `zip_source_get_file_attributes` of `src_final`: the compression
    /// layer's over the source's, and the encryption layer's over both.
    pub(crate) fn attributes(&self) -> Attributes {
        let mut a = self.below;
        if let Some(c) = &self.compressor {
            a = c.attributes().over(a);
        }
        if let Some(s) = &self.sealer {
            a = s.attributes().over(a);
        }
        a
    }

    /// The method the data is in, once it has ended.
    pub(crate) fn method(&self) -> u16 {
        self.compressor.as_ref().map_or(0, Compressor::method)
    }

    /// The CRC and the size of what the source gave.
    pub(crate) fn crc_and_size(&self) -> (u32, u64) {
        (self.lower.hasher.clone().finalize(), self.lower.read)
    }

    /// How much the pipe has given, the entry's compressed size.
    pub(crate) fn written(&self) -> u64 {
        self.written
    }
}
