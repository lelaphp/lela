//! What php's `image_info_type` holds on the request's heap while a read
//! runs: the file sections `exif_file_sections_add` keeps, the tag lists
//! `exif_alloc_image_info_data` grows and the values `exif_iif_add_value`
//! copies, the thumbnail's buffer. The engine keeps the image's values in
//! its own structures; these blocks are php's sizes laid on the request's
//! heap at php's asks, so the memory limit, `memory_get_usage()` and
//! `ZEND_HAS_OOM()` see what php's see, and `exif_discard_imageinfo`
//! gives them back.

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Emalloc;

/// `sizeof(file_section)` on a 64-bit build: an `int`, a `size_t` and
/// a pointer.
const FILE_SECTION: usize = 24;
/// `sizeof(image_info_data)`: two `WORD`s, two `DWORD`s, a pointer and
/// an `image_info_value`.
const INFO_DATA: usize = 32;
/// `sizeof(image_info_value)`, a union of eight bytes.
pub(crate) const INFO_VALUE: usize = 8;

/// A list php grows by doubling its `alloc_count` (`safe_erealloc(list,
/// new_alloc_count, size, 0)`).
#[derive(Debug, Default)]
struct Doubling {
    block: Option<Emalloc>,
    alloc: usize,
    count: usize,
}

impl Doubling {
    /// One more element of `size` bytes, the list grown first where it
    /// is full; the `Err` is the memory limit's end.
    fn push(&mut self, req: &mut Request, size: usize) -> Result<(), Thrown> {
        if self.count == self.alloc {
            let alloc = if self.alloc == 0 { 1 } else { self.alloc * 2 };
            let len = alloc.saturating_mul(size);
            self.block = Some(match self.block.take() {
                None => Emalloc::new(req, len)?,
                Some(block) => block.resize(req, len)?,
            });
            self.alloc = alloc;
        }
        self.count += 1;
        Ok(())
    }

    fn free(&mut self, req: &mut Request) {
        if let Some(block) = self.block.take() {
            block.free(req);
        }
        self.alloc = 0;
        self.count = 0;
    }
}

/// The blocks one read holds, freed together by [`Held::free`].
#[derive(Debug, Default)]
pub(crate) struct Held {
    /// `file.list`.
    sections: Doubling,
    /// Each `file.list[i].data`, `None` for a section of no bytes.
    section_data: Vec<Option<Emalloc>>,
    /// `info_list[i].list`, one per section.
    lists: [Doubling; 14],
    /// The names, values and buffers, each its own `emalloc`.
    blocks: Vec<Emalloc>,
    /// `Thumbnail.data`.
    thumbnail: Option<Emalloc>,
}

impl Held {
    /// `exif_file_sections_add(ImageInfo, type, size, NULL)`: the list
    /// grown where it is full, then `safe_emalloc(size, 1, 0)` for a
    /// section of any bytes; the section's number.
    pub(crate) fn section(&mut self, req: &mut Request, size: usize) -> Result<usize, Thrown> {
        self.sections.push(req, FILE_SECTION)?;
        let data = if size == 0 {
            None
        } else {
            Some(Emalloc::new(req, size)?)
        };
        self.section_data.push(data);
        Ok(self.section_data.len() - 1)
    }

    /// `exif_file_sections_realloc`: the section's bytes grown to `size`
    /// with `safe_ferealloc(..., fallible: true)` since 8.5.12, `false`
    /// where the memory limit refused it.
    pub(crate) fn section_resized(
        &mut self,
        req: &mut Request,
        at: usize,
        size: usize,
    ) -> Result<bool, Thrown> {
        let Some(slot) = self.section_data.get_mut(at) else {
            return Ok(false);
        };
        let held = slot.take();
        let grown = req.memory_mut().fallible(|req| match held {
            None => Emalloc::new(req, size),
            Some(block) => block.resize(req, size),
        })?;
        let refused = grown.is_none();
        if let Some(slot) = self.section_data.get_mut(at) {
            *slot = grown;
        }
        Ok(!refused)
    }

    /// `exif_alloc_image_info_data`: one more entry in the section's
    /// list.
    pub(crate) fn entry(&mut self, req: &mut Request, section: usize) -> Result<(), Thrown> {
        match self.lists.get_mut(section) {
            Some(list) => list.push(req, INFO_DATA),
            None => Ok(()),
        }
    }

    /// One `emalloc(len)` the image keeps, a name's `estrdup` or a
    /// value's `estrndup` among them.
    pub(crate) fn block(&mut self, req: &mut Request, len: usize) -> Result<(), Thrown> {
        let block = Emalloc::new(req, len)?;
        self.blocks.push(block);
        Ok(())
    }

    /// One `safe_femalloc(len, 1, 0, fallible: true)` the image keeps,
    /// `false` where the memory limit refused it.
    pub(crate) fn block_fallibly(&mut self, req: &mut Request, len: usize) -> Result<bool, Thrown> {
        match req.memory_mut().fallible(|req| Emalloc::new(req, len))? {
            Some(block) => {
                self.blocks.push(block);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// `Thumbnail.data = estrndup(thumbnail, size)`, `len` its bytes and
    /// the NUL.
    pub(crate) fn thumbnail(&mut self, req: &mut Request, len: usize) -> Result<(), Thrown> {
        self.thumbnail_free(req);
        self.thumbnail = Some(Emalloc::new(req, len)?);
        Ok(())
    }

    /// `Thumbnail.data = safe_femalloc(size, 1, 0, fallible: true)`,
    /// `false` where the memory limit refused it.
    pub(crate) fn thumbnail_fallibly(
        &mut self,
        req: &mut Request,
        len: usize,
    ) -> Result<bool, Thrown> {
        self.thumbnail_free(req);
        self.thumbnail = req.memory_mut().fallible(|req| Emalloc::new(req, len))?;
        Ok(self.thumbnail.is_some())
    }

    /// `exif_thumbnail_build`'s `safe_erealloc` of the thumbnail to `len`.
    pub(crate) fn thumbnail_resize(&mut self, req: &mut Request, len: usize) -> Result<(), Thrown> {
        self.thumbnail = Some(match self.thumbnail.take() {
            None => Emalloc::new(req, len)?,
            Some(block) => block.resize(req, len)?,
        });
        Ok(())
    }

    /// `efree(Thumbnail.data)`.
    pub(crate) fn thumbnail_free(&mut self, req: &mut Request) {
        if let Some(block) = self.thumbnail.take() {
            block.free(req);
        }
    }

    /// `exif_discard_imageinfo`'s frees.
    pub(crate) fn free(&mut self, req: &mut Request) {
        self.thumbnail_free(req);
        for data in std::mem::take(&mut self.section_data).into_iter().flatten() {
            data.free(req);
        }
        self.sections.free(req);
        for list in &mut self.lists {
            list.free(req);
        }
        for block in std::mem::take(&mut self.blocks) {
            block.free(req);
        }
    }
}
