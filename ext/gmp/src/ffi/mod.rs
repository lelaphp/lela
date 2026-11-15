//! libgmp, the library the pinned php links (design §7.2, revision 72):
//! the few dozen `mpz_*` and `gmp_rand*` calls `gmp.c` makes,
//! declared by hand under the `__gmpz_*` and `__gmp_*` names `gmp.h`'s
//! macros map them to, behind [`Mpz`], which owns one `mpz_t` from
//! `mpz_init` to `mpz_clear`, and [`RandState`], which owns one
//! `gmp_randstate_t`. php sets no `mp_set_memory_functions`, so libgmp
//! allocates the limbs with `malloc` and `memory_get_usage()` counts
//! none of them; neither does this binding. The one home of this
//! crate's `unsafe` (`unsafe.homes`' `ext/*/src/ffi/`).
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, c_char, c_int, c_long, c_ulong, c_void};

mod sys {
    use std::ffi::{c_char, c_double, c_int, c_long, c_ulong, c_void};

    /// `__mpz_struct`.
    #[repr(C)]
    pub(super) struct Mpz {
        pub(super) alloc: c_int,
        pub(super) size: c_int,
        pub(super) limbs: *mut u64,
    }

    /// `__gmp_randstate_struct`: the seed, the algorithm and its data.
    #[repr(C)]
    pub(super) struct RandState {
        pub(super) seed: Mpz,
        pub(super) alg: c_int,
        pub(super) data: *mut c_void,
    }

    pub(super) type Ptr = *mut Mpz;
    pub(super) type Src = *const Mpz;

    #[link(name = "gmp")]
    unsafe extern "C" {
        #[link_name = "__gmp_version"]
        pub(super) static VERSION: *const c_char;
        #[link_name = "__gmpz_init"]
        pub(super) fn init(x: Ptr);
        #[link_name = "__gmpz_clear"]
        pub(super) fn clear(x: Ptr);
        #[link_name = "__gmpz_set"]
        pub(super) fn set(r: Ptr, x: Src);
        #[link_name = "__gmpz_set_si"]
        pub(super) fn set_si(r: Ptr, x: c_long);
        #[link_name = "__gmpz_set_str"]
        pub(super) fn set_str(r: Ptr, s: *const c_char, base: c_int) -> c_int;
        #[link_name = "__gmpz_get_si"]
        pub(super) fn get_si(x: Src) -> c_long;
        #[link_name = "__gmpz_get_ui"]
        pub(super) fn get_ui(x: Src) -> c_ulong;
        #[link_name = "__gmpz_get_d"]
        pub(super) fn get_d(x: Src) -> c_double;
        #[link_name = "__gmpz_get_str"]
        pub(super) fn get_str(s: *mut c_char, base: c_int, x: Src) -> *mut c_char;
        #[link_name = "__gmpz_sizeinbase"]
        pub(super) fn sizeinbase(x: Src, base: c_int) -> usize;
        #[link_name = "__gmpz_fits_slong_p"]
        pub(super) fn fits_slong_p(x: Src) -> c_int;
        #[link_name = "__gmpz_cmp"]
        pub(super) fn cmp(a: Src, b: Src) -> c_int;
        #[link_name = "__gmpz_import"]
        pub(super) fn import(
            r: Ptr,
            count: usize,
            order: c_int,
            size: usize,
            endian: c_int,
            nails: usize,
            data: *const c_void,
        );
        #[link_name = "__gmpz_export"]
        pub(super) fn export(
            out: *mut c_void,
            count: *mut usize,
            order: c_int,
            size: usize,
            endian: c_int,
            nails: usize,
            x: Src,
        ) -> *mut c_void;
        #[link_name = "__gmpz_tdiv_qr"]
        pub(super) fn tdiv_qr(q: Ptr, r: Ptr, n: Src, d: Src);
        #[link_name = "__gmpz_cdiv_qr"]
        pub(super) fn cdiv_qr(q: Ptr, r: Ptr, n: Src, d: Src);
        #[link_name = "__gmpz_fdiv_qr"]
        pub(super) fn fdiv_qr(q: Ptr, r: Ptr, n: Src, d: Src);
        #[link_name = "__gmpz_sqrtrem"]
        pub(super) fn sqrtrem(s: Ptr, r: Ptr, x: Src);
        #[link_name = "__gmpz_rootrem"]
        pub(super) fn rootrem(s: Ptr, r: Ptr, x: Src, n: c_ulong);
        #[link_name = "__gmpz_gcdext"]
        pub(super) fn gcdext(g: Ptr, s: Ptr, t: Ptr, a: Src, b: Src);
        #[link_name = "__gmpz_powm"]
        pub(super) fn powm(r: Ptr, b: Src, e: Src, m: Src);
        #[link_name = "__gmpz_invert"]
        pub(super) fn invert(r: Ptr, a: Src, b: Src) -> c_int;
        #[link_name = "__gmpz_setbit"]
        pub(super) fn setbit(r: Ptr, bit: c_ulong);
        #[link_name = "__gmpz_clrbit"]
        pub(super) fn clrbit(r: Ptr, bit: c_ulong);
        #[link_name = "__gmpz_tstbit"]
        pub(super) fn tstbit(x: Src, bit: c_ulong) -> c_int;
        #[link_name = "__gmpz_fac_ui"]
        pub(super) fn fac_ui(r: Ptr, n: c_ulong);
        #[link_name = "__gmp_randinit_mt"]
        pub(super) fn randinit_mt(s: *mut RandState);
        #[link_name = "__gmp_randseed_ui"]
        pub(super) fn randseed_ui(s: *mut RandState, seed: c_ulong);
        #[link_name = "__gmp_randseed"]
        pub(super) fn randseed(s: *mut RandState, seed: Src);
        #[link_name = "__gmp_randclear"]
        pub(super) fn randclear(s: *mut RandState);
        #[link_name = "__gmpz_urandomb"]
        pub(super) fn urandomb(r: Ptr, s: *mut RandState, bits: c_ulong);
        #[link_name = "__gmpz_urandomm"]
        pub(super) fn urandomm(r: Ptr, s: *mut RandState, n: Src);
    }
}

/// One `mpz_t`, `mpz_init` to `mpz_clear`. The struct holds a pointer to
/// its limbs and nothing that points back at it, so it moves.
pub(crate) struct Mpz(sys::Mpz);

// SAFETY: an `mpz_t` owns its limbs alone and libgmp keeps no state of
// a thread's, so the value may move to another thread with its owner.
unsafe impl Send for Mpz {}

impl Default for Mpz {
    fn default() -> Mpz {
        let mut x = sys::Mpz {
            alloc: 0,
            size: 0,
            limbs: std::ptr::null_mut(),
        };
        // SAFETY: `x` is a struct of `__mpz_struct`'s layout this frame
        // owns, which `mpz_init` lays down.
        unsafe { sys::init(&raw mut x) };
        Mpz(x)
    }
}

impl Drop for Mpz {
    fn drop(&mut self) {
        // SAFETY: `self` was initialised and is cleared once, here.
        unsafe { sys::clear(&raw mut self.0) };
    }
}

impl Clone for Mpz {
    fn clone(&self) -> Mpz {
        let mut copy = Mpz::default();
        copy.set(self);
        copy
    }
}

impl Mpz {
    fn ptr(&mut self) -> *mut sys::Mpz {
        &raw mut self.0
    }

    fn src(&self) -> *const sys::Mpz {
        &raw const self.0
    }

    /// `mpz_set_si`.
    pub(crate) fn from_long(n: i64) -> Mpz {
        let mut x = Mpz::default();
        // SAFETY: `x` is initialised.
        unsafe { sys::set_si(x.ptr(), c_long::from(n)) };
        x
    }

    /// `mpz_set`.
    pub(crate) fn set(&mut self, x: &Mpz) {
        // SAFETY: both are initialised; libgmp allows `self` and `x` to
        // be one, which the borrows rule out anyway.
        unsafe { sys::set(self.ptr(), x.src()) };
    }

    /// `mpz_set_str` over the text up to its first NUL, as php hands
    /// libgmp a `zend_string`'s bytes; `false` is libgmp's `-1`.
    pub(crate) fn set_str(&mut self, text: &[u8], base: i32) -> bool {
        let mut owned = php_ext::coerce::c_str(text).to_vec();
        owned.push(0);
        // SAFETY: `owned` is NUL-terminated and outlives the call.
        let done = unsafe { sys::set_str(self.ptr(), owned.as_ptr().cast(), base) };
        done == 0
    }

    /// `mpz_sgn`, the macro over `_mp_size`.
    pub(crate) fn sign(&self) -> i64 {
        i64::from(self.0.size.signum())
    }

    /// `mpz_get_si`; a C `long` is php's `zend_long` on the 64-bit
    /// targets.
    #[allow(clippy::useless_conversion)]
    pub(crate) fn to_long(&self) -> i64 {
        // SAFETY: `self` is initialised.
        i64::from(unsafe { sys::get_si(self.src()) })
    }

    /// `mpz_get_ui`.
    #[allow(clippy::useless_conversion)]
    pub(crate) fn to_ulong(&self) -> u64 {
        // SAFETY: `self` is initialised.
        u64::from(unsafe { sys::get_ui(self.src()) })
    }

    /// `mpz_get_d`.
    pub(crate) fn to_double(&self) -> f64 {
        // SAFETY: `self` is initialised.
        unsafe { sys::get_d(self.src()) }
    }

    /// `mpz_fits_slong_p`.
    pub(crate) fn fits_long(&self) -> bool {
        // SAFETY: `self` is initialised.
        unsafe { sys::fits_slong_p(self.src()) != 0 }
    }

    /// `mpz_sizeinbase`.
    pub(crate) fn size_in_base(&self, base: i32) -> usize {
        // SAFETY: `self` is initialised.
        unsafe { sys::sizeinbase(self.src(), base) }
    }

    /// `mpz_cmp`.
    pub(crate) fn cmp(&self, other: &Mpz) -> std::cmp::Ordering {
        // SAFETY: both are initialised.
        unsafe { sys::cmp(self.src(), other.src()) }.cmp(&0)
    }

    /// `mpz_get_str` into a buffer of `mpz_sizeinbase` digits, a sign
    /// and the NUL, as `gmp_strval` sizes its string: the digits up to
    /// the NUL.
    pub(crate) fn to_text(&self, base: i32) -> Vec<u8> {
        let len = self.size_in_base(base.abs()) + 2;
        let mut out = vec![0u8; len];
        // SAFETY: `out` holds the digits, a sign and the NUL that
        // `mpz_get_str` writes at most.
        unsafe { sys::get_str(out.as_mut_ptr().cast::<c_char>(), base, self.src()) };
        let end = out.iter().position(|b| *b == 0).unwrap_or(out.len());
        out.truncate(end);
        out
    }

    /// `mpz_import` of `data` in words of `size` bytes.
    pub(crate) fn import(data: &[u8], size: usize, order: i32, endian: i32) -> Mpz {
        let mut x = Mpz::default();
        let count = data.len().checked_div(size).unwrap_or(0);
        // SAFETY: `data` holds `count` words of `size` bytes.
        unsafe { sys::import(x.ptr(), count, order, size, endian, 0, data.as_ptr().cast()) };
        x
    }

    /// `mpz_export` into `count` words of `size` bytes, `count` being
    /// what `gmp_export` computes from `mpz_sizeinbase(x, 2)`.
    pub(crate) fn export(&self, count: usize, size: usize, order: i32, endian: i32) -> Vec<u8> {
        let mut out = vec![0u8; count.saturating_mul(size)];
        let mut written = 0usize;
        // SAFETY: `out` holds the `count` words of `size` bytes the
        // number's bits fill.
        unsafe {
            sys::export(
                out.as_mut_ptr().cast::<c_void>(),
                &raw mut written,
                order,
                size,
                endian,
                0,
                self.src(),
            )
        };
        out
    }

    /// `mpz_setbit`.
    pub(crate) fn set_bit(&mut self, bit: u64) {
        // SAFETY: `self` is initialised.
        unsafe { sys::setbit(self.ptr(), c_ulong::from(bit)) };
    }

    /// `mpz_clrbit`.
    pub(crate) fn clear_bit(&mut self, bit: u64) {
        // SAFETY: `self` is initialised.
        unsafe { sys::clrbit(self.ptr(), c_ulong::from(bit)) };
    }

    /// `mpz_tstbit`.
    pub(crate) fn test_bit(&self, bit: u64) -> bool {
        // SAFETY: `self` is initialised.
        unsafe { sys::tstbit(self.src(), c_ulong::from(bit)) != 0 }
    }

    /// `mpz_fac_ui`.
    pub(crate) fn factorial(n: u64) -> Mpz {
        let mut x = Mpz::default();
        // SAFETY: `x` is initialised.
        unsafe { sys::fac_ui(x.ptr(), c_ulong::from(n)) };
        x
    }

    /// `mpz_powm`.
    pub(crate) fn powm(&self, exp: &Mpz, modulus: &Mpz) -> Mpz {
        let mut r = Mpz::default();
        // SAFETY: all three are initialised and `r` is apart from them.
        unsafe { sys::powm(r.ptr(), self.src(), exp.src(), modulus.src()) };
        r
    }

    /// `mpz_invert`, `None` where there is no inverse.
    pub(crate) fn invert(&self, modulus: &Mpz) -> Option<Mpz> {
        let mut r = Mpz::default();
        // SAFETY: both are initialised and `r` is apart from them.
        let found = unsafe { sys::invert(r.ptr(), self.src(), modulus.src()) };
        (found != 0).then_some(r)
    }

    /// `mpz_gcdext`: `g`, `s` and `t`.
    pub(crate) fn gcdext(&self, b: &Mpz) -> (Mpz, Mpz, Mpz) {
        let (mut g, mut s, mut t) = (Mpz::default(), Mpz::default(), Mpz::default());
        // SAFETY: all five are initialised and the three results apart.
        unsafe { sys::gcdext(g.ptr(), s.ptr(), t.ptr(), self.src(), b.src()) };
        (g, s, t)
    }

    /// One of the `mpz_{t,c,f}div_qr`, as `round` names them: the
    /// quotient and the remainder.
    pub(crate) fn div_qr(&self, d: &Mpz, round: Round) -> (Mpz, Mpz) {
        let call = match round {
            Round::Zero => sys::tdiv_qr,
            Round::PlusInf => sys::cdiv_qr,
            Round::MinusInf => sys::fdiv_qr,
        };
        let (mut q, mut r) = (Mpz::default(), Mpz::default());
        // SAFETY: all four are initialised and the two results apart.
        unsafe { call(q.ptr(), r.ptr(), self.src(), d.src()) };
        (q, r)
    }

    /// `mpz_sqrtrem`.
    pub(crate) fn sqrtrem(&self) -> (Mpz, Mpz) {
        let (mut s, mut r) = (Mpz::default(), Mpz::default());
        // SAFETY: all three are initialised and the two results apart.
        unsafe { sys::sqrtrem(s.ptr(), r.ptr(), self.src()) };
        (s, r)
    }

    /// `mpz_rootrem`.
    pub(crate) fn rootrem(&self, n: u64) -> (Mpz, Mpz) {
        let (mut s, mut r) = (Mpz::default(), Mpz::default());
        // SAFETY: all three are initialised and the two results apart.
        unsafe { sys::rootrem(s.ptr(), r.ptr(), self.src(), c_ulong::from(n)) };
        (s, r)
    }
}

/// `GMP_ROUND_ZERO`, `GMP_ROUND_PLUSINF` and `GMP_ROUND_MINUSINF`: which
/// of libgmp's divisions a `gmp_div*` call makes.
#[derive(Clone, Copy)]
pub(crate) enum Round {
    Zero,
    PlusInf,
    MinusInf,
}

/// Each call of one shape as a method of [`Mpz`] answering a fresh
/// number, so what a call writes is never one of its operands.
macro_rules! calls {
    ($($kind:ident $name:ident = $link:literal;)*) => {
        mod raw {
            use std::ffi::{c_int, c_ulong};
            use super::sys::{Ptr, Src};
            #[link(name = "gmp")]
            unsafe extern "C" {
                $(calls!(@extern $kind $name $link);)*
            }
        }
        impl Mpz {
            $(calls!(@one $kind $name);)*
        }
    };
    (@extern binary $name:ident $link:literal) => {
        #[link_name = $link]
        pub(super) fn $name(r: Ptr, a: Src, b: Src);
    };
    (@extern unary $name:ident $link:literal) => {
        #[link_name = $link]
        pub(super) fn $name(r: Ptr, a: Src);
    };
    (@extern by_ui $name:ident $link:literal) => {
        #[link_name = $link]
        pub(super) fn $name(r: Ptr, a: Src, n: c_ulong);
    };
    (@extern test $name:ident $link:literal) => {
        #[link_name = $link]
        pub(super) fn $name(a: Src) -> c_int;
    };
    (@extern pair $name:ident $link:literal) => {
        #[link_name = $link]
        pub(super) fn $name(a: Src, b: Src) -> c_int;
    };
    (@extern count $name:ident $link:literal) => {
        #[link_name = $link]
        pub(super) fn $name(a: Src, n: c_ulong) -> c_ulong;
    };
    (@one binary $name:ident) => {
        #[doc = concat!("`mpz_", stringify!($name), "`.")]
        pub(crate) fn $name(&self, b: &Mpz) -> Mpz {
            let mut r = Mpz::default();
            // SAFETY: both operands are initialised and `r` is apart.
            unsafe { raw::$name(r.ptr(), self.src(), b.src()) };
            r
        }
    };
    (@one unary $name:ident) => {
        #[doc = concat!("`mpz_", stringify!($name), "`.")]
        pub(crate) fn $name(&self) -> Mpz {
            let mut r = Mpz::default();
            // SAFETY: the operand is initialised and `r` is apart.
            unsafe { raw::$name(r.ptr(), self.src()) };
            r
        }
    };
    (@one by_ui $name:ident) => {
        #[doc = concat!("`mpz_", stringify!($name), "`.")]
        pub(crate) fn $name(&self, n: u64) -> Mpz {
            let mut r = Mpz::default();
            // SAFETY: the operand is initialised and `r` is apart.
            unsafe { raw::$name(r.ptr(), self.src(), c_ulong::from(n)) };
            r
        }
    };
    (@one test $name:ident) => {
        #[doc = concat!("`mpz_", stringify!($name), "`.")]
        pub(crate) fn $name(&self) -> i64 {
            // SAFETY: the operand is initialised.
            i64::from(unsafe { raw::$name(self.src()) })
        }
    };
    (@one pair $name:ident) => {
        #[doc = concat!("`mpz_", stringify!($name), "`.")]
        pub(crate) fn $name(&self, b: &Mpz) -> i64 {
            // SAFETY: both operands are initialised.
            i64::from(unsafe { raw::$name(self.src(), b.src()) })
        }
    };
    (@one count $name:ident) => {
        #[doc = concat!("`mpz_", stringify!($name), "`, as php's `zend_long`.")]
        #[allow(clippy::cast_possible_wrap)]
        pub(crate) fn $name(&self, n: u64) -> i64 {
            // SAFETY: the operand is initialised.
            (unsafe { raw::$name(self.src(), c_ulong::from(n)) }) as i64
        }
    };
}

calls! {
    binary add = "__gmpz_add";
    binary sub = "__gmpz_sub";
    binary mul = "__gmpz_mul";
    binary tdiv_q = "__gmpz_tdiv_q";
    binary tdiv_r = "__gmpz_tdiv_r";
    binary cdiv_q = "__gmpz_cdiv_q";
    binary cdiv_r = "__gmpz_cdiv_r";
    binary fdiv_q = "__gmpz_fdiv_q";
    binary fdiv_r = "__gmpz_fdiv_r";
    binary modulo = "__gmpz_mod";
    binary divexact = "__gmpz_divexact";
    binary gcd = "__gmpz_gcd";
    binary lcm = "__gmpz_lcm";
    binary and = "__gmpz_and";
    binary ior = "__gmpz_ior";
    binary xor = "__gmpz_xor";
    unary neg = "__gmpz_neg";
    unary abs = "__gmpz_abs";
    unary com = "__gmpz_com";
    unary nextprime = "__gmpz_nextprime";
    unary sqrt = "__gmpz_sqrt";
    by_ui pow_ui = "__gmpz_pow_ui";
    by_ui root = "__gmpz_root";
    by_ui bin_ui = "__gmpz_bin_ui";
    by_ui mul_2exp = "__gmpz_mul_2exp";
    by_ui fdiv_q_2exp = "__gmpz_fdiv_q_2exp";
    test perfect_square_p = "__gmpz_perfect_square_p";
    test perfect_power_p = "__gmpz_perfect_power_p";
    pair jacobi = "__gmpz_jacobi";
    count scan0 = "__gmpz_scan0";
    count scan1 = "__gmpz_scan1";
}

impl Mpz {
    /// `mpz_probab_prime_p`.
    pub(crate) fn probab_prime(&self, reps: i32) -> i64 {
        // SAFETY: the operand is initialised.
        i64::from(unsafe { probab_prime_p(self.src(), reps) })
    }

    /// `mpz_popcount`, as php's `zend_long`: all ones where the number
    /// is negative, which `RETURN_LONG` reads as -1.
    #[allow(clippy::cast_possible_wrap)]
    pub(crate) fn popcount(&self) -> i64 {
        // SAFETY: the operand is initialised.
        (unsafe { popcount(self.src()) }) as i64
    }

    /// `mpz_hamdist`, read as [`Mpz::popcount`] is.
    #[allow(clippy::cast_possible_wrap)]
    pub(crate) fn hamdist(&self, b: &Mpz) -> i64 {
        // SAFETY: both operands are initialised.
        (unsafe { hamdist(self.src(), b.src()) }) as i64
    }
}

#[link(name = "gmp")]
unsafe extern "C" {
    #[link_name = "__gmpz_probab_prime_p"]
    fn probab_prime_p(x: *const sys::Mpz, reps: c_int) -> c_int;
    #[link_name = "__gmpz_popcount"]
    fn popcount(x: *const sys::Mpz) -> c_ulong;
    #[link_name = "__gmpz_hamdist"]
    fn hamdist(a: *const sys::Mpz, b: *const sys::Mpz) -> c_ulong;
}

/// One `gmp_randstate_t`, `gmp_randinit_mt` to `gmp_randclear`, on the
/// heap so libgmp's pointers into it hold wherever its owner moves.
pub(crate) struct RandState(Box<sys::RandState>);

// SAFETY: the state owns its seed and its Mersenne Twister buffer alone,
// and libgmp keeps no state of a thread's.
unsafe impl Send for RandState {}

impl RandState {
    /// `gmp_randinit_mt` then `gmp_randseed_ui(seed)`.
    pub(crate) fn mersenne_twister(seed: u64) -> RandState {
        let mut state = Box::new(sys::RandState {
            seed: sys::Mpz {
                alloc: 0,
                size: 0,
                limbs: std::ptr::null_mut(),
            },
            alg: 0,
            data: std::ptr::null_mut(),
        });
        // SAFETY: `state` is a `gmp_randstate_t`'s layout, which
        // `gmp_randinit_mt` lays down.
        unsafe { sys::randinit_mt(&raw mut *state) };
        // SAFETY: `state` is initialised.
        unsafe { sys::randseed_ui(&raw mut *state, c_ulong::from(seed)) };
        RandState(state)
    }

    /// `gmp_randseed`.
    pub(crate) fn seed(&mut self, seed: &Mpz) {
        // SAFETY: both are initialised.
        unsafe { sys::randseed(&raw mut *self.0, seed.src()) };
    }

    /// `mpz_urandomb`.
    pub(crate) fn bits(&mut self, bits: u64) -> Mpz {
        let mut r = Mpz::default();
        // SAFETY: both are initialised.
        unsafe { sys::urandomb(r.ptr(), &raw mut *self.0, c_ulong::from(bits)) };
        r
    }

    /// `mpz_urandomm`.
    pub(crate) fn below(&mut self, n: &Mpz) -> Mpz {
        let mut r = Mpz::default();
        // SAFETY: all three are initialised and `r` is apart from `n`.
        unsafe { sys::urandomm(r.ptr(), &raw mut *self.0, n.src()) };
        r
    }
}

impl Drop for RandState {
    fn drop(&mut self) {
        // SAFETY: the state was initialised and is cleared once, here.
        unsafe { sys::randclear(&raw mut *self.0) };
    }
}

/// `gmp_version`, the library's own string: `GMP_VERSION` and the
/// `--ri` row.
pub(crate) fn version() -> String {
    // SAFETY: `__gmp_version` is a constant libgmp sets before any call.
    let text = unsafe { sys::VERSION };
    if text.is_null() {
        return String::new();
    }
    // SAFETY: the pointer is libgmp's NUL-terminated constant string.
    let text = unsafe { CStr::from_ptr(text) };
    text.to_string_lossy().into_owned()
}

/// `php_random_bytes_silent(&seed, sizeof(seed))`: eight bytes of the
/// platform's entropy, `None` where it gives none.
pub(crate) fn entropy() -> Option<u64> {
    let mut seed = [0u8; 8];
    // SAFETY: `seed` holds the eight bytes asked for.
    let done = unsafe { getentropy(seed.as_mut_ptr().cast(), seed.len()) };
    (done == 0).then(|| u64::from_ne_bytes(seed))
}

// `getentropy(3)`, which macOS, glibc and musl each give in `unistd.h`;
// declared here because the libc crate leaves it out for musl.
unsafe extern "C" {
    fn getentropy(buffer: *mut c_void, length: libc::size_t) -> c_int;
}
