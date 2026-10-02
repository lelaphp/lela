//! `uidna.h`: the UTS #46 processor `idn_to_ascii` and `idn_to_utf8`
//! open, over UTF-8 names.

use std::ffi::c_char;

use super::{Code, length};

mod sys {
    use std::ffi::c_char;

    use super::super::Code;

    /// `UIDNA`, opaque.
    pub(crate) enum Uidna {}

    /// `UIDNAInfo`.
    #[repr(C)]
    pub(crate) struct UidnaInfo {
        pub(crate) size: i16,
        pub(crate) is_transitional_different: i8,
        pub(crate) reserved_b3: i8,
        pub(crate) errors: u32,
        pub(crate) reserved_i2: i32,
        pub(crate) reserved_i3: i32,
    }

    icu! {
        fn uidna_openUTS46(options: u32, status: &mut Code) -> *mut Uidna;
        fn uidna_close(idna: *mut Uidna);
        fn uidna_nameToASCII_UTF8(
            idna: *const Uidna,
            name: *const c_char,
            len: i32,
            dest: *mut c_char,
            capacity: i32,
            info: *mut UidnaInfo,
            status: &mut Code,
        ) -> i32;
        fn uidna_nameToUnicodeUTF8(
            idna: *const Uidna,
            name: *const c_char,
            len: i32,
            dest: *mut c_char,
            capacity: i32,
            info: *mut UidnaInfo,
            status: &mut Code,
        ) -> i32;
    }
}

/// One conversion: the bytes ICU wrote (cut to the buffer), the length it
/// answered, its code, and `UIDNAInfo`'s two fields php reads.
pub(crate) struct Converted {
    pub(crate) bytes: Vec<u8>,
    pub(crate) len: i32,
    pub(crate) status: Code,
    pub(crate) transitional_different: bool,
    pub(crate) errors: u32,
}

/// A `UIDNA` from `uidna_openUTS46` to `uidna_close`.
pub(crate) struct Idna(*mut sys::Uidna);

impl Idna {
    /// `uidna_openUTS46(options)`.
    pub(crate) fn open(options: u32) -> Result<Idna, Code> {
        let mut status = Code::ZERO;
        // SAFETY: the call takes the options and a status.
        let idna = unsafe { sys::uidna_openUTS46(options, &mut status) };
        status.check()?;
        Ok(Idna(idna))
    }

    /// `uidna_nameToASCII_UTF8` (or `uidna_nameToUnicodeUTF8`) into a
    /// buffer of `capacity` bytes, as php allocates it.
    pub(crate) fn convert(&self, name: &[u8], to_ascii: bool, capacity: i32) -> Converted {
        let mut info = sys::UidnaInfo {
            size: 16,
            is_transitional_different: 0,
            reserved_b3: 0,
            errors: 0,
            reserved_i2: 0,
            reserved_i3: 0,
        };
        let mut status = Code::ZERO;
        let mut buffer = vec![0u8; usize::try_from(capacity).unwrap_or(0) + 1];
        let src_len = length(name.len()).unwrap_or(i32::MAX);
        let dest = buffer.as_mut_ptr().cast::<c_char>();
        let call = if to_ascii {
            sys::uidna_nameToASCII_UTF8
        } else {
            sys::uidna_nameToUnicodeUTF8
        };
        // SAFETY: the processor is open, the name a live slice of src_len
        // bytes, dest holds capacity bytes and ICU writes no further.
        let len = unsafe {
            call(
                self.0,
                name.as_ptr().cast(),
                src_len,
                dest,
                capacity,
                &raw mut info,
                &mut status,
            )
        };
        buffer.truncate(usize::try_from(len).unwrap_or(0).min(buffer.len()));
        Converted {
            bytes: buffer,
            len,
            status,
            transitional_different: info.is_transitional_different != 0,
            errors: info.errors,
        }
    }
}

impl Drop for Idna {
    fn drop(&mut self) {
        // SAFETY: the processor is open and closed once, here.
        unsafe { sys::uidna_close(self.0) };
    }
}
