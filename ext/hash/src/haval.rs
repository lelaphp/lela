//! `hash_haval.c`: HAVAL at three, four or five passes and five digest
//! lengths, over php's 128-byte `PHP_HAVAL_CTX`, `l8l2b128`.
//
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use crate::algos::{Slot, State, le_words};
use crate::md::feed;

/// `D0` and `K2` to `K5`: the fraction of pi, word by word.
#[rustfmt::skip]
#[allow(clippy::unreadable_literal)]
const PI: [u32; 136] = [
    0x243F6A88, 0x85A308D3, 0x13198A2E, 0x03707344, 0xA4093822, 0x299F31D0, 0x082EFA98, 0xEC4E6C89,
    0x452821E6, 0x38D01377, 0xBE5466CF, 0x34E90C6C, 0xC0AC29B7, 0xC97C50DD, 0x3F84D5B5, 0xB5470917,
    0x9216D5D9, 0x8979FB1B, 0xD1310BA6, 0x98DFB5AC, 0x2FFD72DB, 0xD01ADFB7, 0xB8E1AFED, 0x6A267E96,
    0xBA7C9045, 0xF12C7F99, 0x24A19947, 0xB3916CF7, 0x0801F2E2, 0x858EFC16, 0x636920D8, 0x71574E69,
    0xA458FEA3, 0xF4933D7E, 0x0D95748F, 0x728EB658, 0x718BCD58, 0x82154AEE, 0x7B54A41D, 0xC25A59B5,
    0x9C30D539, 0x2AF26013, 0xC5D1B023, 0x286085F0, 0xCA417918, 0xB8DB38EF, 0x8E79DCB0, 0x603A180E,
    0x6C9E0E8B, 0xB01E8A3E, 0xD71577C1, 0xBD314B27, 0x78AF2FDA, 0x55605C60, 0xE65525F3, 0xAA55AB94,
    0x57489862, 0x63E81440, 0x55CA396A, 0x2AAB10B6, 0xB4CC5C34, 0x1141E8CE, 0xA15486AF, 0x7C72E993,
    0xB3EE1411, 0x636FBC2A, 0x2BA9C55D, 0x741831F6, 0xCE5C3E16, 0x9B87931E, 0xAFD6BA33, 0x6C24CF5C,
    0x7A325381, 0x28958677, 0x3B8F4898, 0x6B4BB9AF, 0xC4BFE81B, 0x66282193, 0x61D809CC, 0xFB21A991,
    0x487CAC60, 0x5DEC8032, 0xEF845D5D, 0xE98575B1, 0xDC262302, 0xEB651B88, 0x23893E81, 0xD396ACC5,
    0x0F6D6FF3, 0x83F44239, 0x2E0B4482, 0xA4842004, 0x69C8F04A, 0x9E1F9B5E, 0x21C66842, 0xF6E96C9A,
    0x670C9C61, 0xABD388F0, 0x6A51A0D2, 0xD8542F68, 0x960FA728, 0xAB5133A3, 0x6EEF0B6C, 0x137A3BE4,
    0xBA3BF050, 0x7EFB2A98, 0xA1F1651D, 0x39AF0176, 0x66CA593E, 0x82430E88, 0x8CEE8619, 0x456F9FB4,
    0x7D84A5C3, 0x3B8B5EBE, 0xE06F75D8, 0x85C12073, 0x401A449F, 0x56C16AA6, 0x4ED3AA62, 0x363F7706,
    0x1BFEDF72, 0x429B023D, 0x37D0D724, 0xD00A1248, 0xDB0FEAD3, 0x49F1C09B, 0x075372C9, 0x80991B7B,
    0x25D479D8, 0xF6E8DEF7, 0xE3FE501A, 0xB6794C3B, 0x976CE0BD, 0x04C006BA, 0xC1A94FB6, 0x409F60C4,
];

/// `I2` to `I5`: the word order of passes two to five.
#[rustfmt::skip]
const ORDER: [[u8; 32]; 4] = [
    [5, 14, 26, 18, 11, 28, 7, 16, 0, 23, 20, 22, 1, 10, 4, 8, 30, 3, 21, 9, 17, 24, 29, 6, 19, 12, 15, 13, 2, 25, 31, 27],
    [19, 9, 4, 20, 28, 17, 8, 22, 29, 14, 25, 12, 24, 30, 16, 26, 31, 15, 7, 3, 1, 0, 18, 27, 13, 6, 21, 10, 23, 11, 5, 2],
    [24, 4, 0, 14, 2, 7, 28, 23, 26, 6, 30, 20, 18, 25, 19, 3, 22, 11, 31, 21, 8, 27, 12, 9, 1, 29, 5, 15, 17, 10, 16, 13],
    [27, 3, 21, 26, 17, 11, 20, 29, 19, 0, 12, 7, 13, 8, 31, 10, 5, 9, 14, 30, 18, 6, 28, 24, 2, 23, 16, 22, 4, 1, 25, 15],
];

/// The `M` rows each pass of `PHP_<p>HAVALTransform` hands `F1` to
/// `F5`, as `x6` down to `x0`.
#[rustfmt::skip]
const ARGS: [[[u8; 7]; 5]; 3] = [
    [[1, 0, 3, 5, 6, 2, 4], [4, 2, 1, 0, 5, 3, 6], [6, 1, 2, 3, 4, 5, 0], [0; 7], [0; 7]],
    [[2, 6, 1, 4, 5, 3, 0], [3, 5, 2, 0, 1, 6, 4], [1, 4, 3, 6, 0, 2, 5], [6, 4, 0, 5, 2, 1, 3], [0; 7]],
    [[3, 4, 1, 0, 5, 2, 6], [6, 2, 1, 0, 3, 4, 5], [2, 6, 0, 4, 3, 1, 5], [1, 5, 3, 2, 0, 4, 6], [2, 5, 0, 6, 4, 3, 1]],
];

/// `F1` to `F5`.
fn f(n: usize, [x6, x5, x4, x3, x2, x1, x0]: [u32; 7]) -> u32 {
    match n {
        0 => (x1 & x4) ^ (x2 & x5) ^ (x3 & x6) ^ (x0 & x1) ^ x0,
        1 => {
            (x1 & x2 & x3)
                ^ (x2 & x4 & x5)
                ^ (x1 & x2)
                ^ (x1 & x4)
                ^ (x2 & x6)
                ^ (x3 & x5)
                ^ (x4 & x5)
                ^ (x0 & x2)
                ^ x0
        }
        2 => (x1 & x2 & x3) ^ (x1 & x4) ^ (x2 & x5) ^ (x3 & x6) ^ (x0 & x3) ^ x0,
        3 => {
            (x1 & x2 & x3)
                ^ (x2 & x4 & x5)
                ^ (x3 & x4 & x6)
                ^ (x1 & x4)
                ^ (x2 & x6)
                ^ (x3 & x4)
                ^ (x3 & x5)
                ^ (x3 & x6)
                ^ (x4 & x5)
                ^ (x4 & x6)
                ^ (x0 & x4)
                ^ x0
        }
        _ => (x1 & x4) ^ (x2 & x5) ^ (x3 & x6) ^ (x0 & x1 & x2 & x3) ^ (x0 & x5) ^ x0,
    }
}

/// `PHP_<p>HAVALTransform`: `P` passes of 32 steps, `E[7 - i % 8]`
/// written from the seven words the pass's `M` rows pick.
fn transform<const P: usize>(state: &mut [u32; 8], block: &[u8]) {
    let x: [u32; 32] = le_words(block);
    let mut e = *state;
    for pass in 0..P {
        for i in 0..32 {
            let args = ARGS[P - 3][pass].map(|m| e[(8 + usize::from(m) - i % 8) % 8]);
            let word = if pass == 0 {
                x[i]
            } else {
                x[usize::from(ORDER[pass - 1][i])]
            };
            let k = if pass == 0 {
                0
            } else {
                PI[8 + 32 * (pass - 1) + i]
            };
            e[7 - i % 8] = f(pass, args)
                .rotate_right(7)
                .wrapping_add(e[(15 - i % 8) % 8].rotate_right(11))
                .wrapping_add(word)
                .wrapping_add(k);
        }
    }
    for (word, add) in state.iter_mut().zip(e) {
        *word = word.wrapping_add(add);
    }
}

/// `PHP_HAVAL_CTX` at `P` passes and `B` bits.
///
/// ```php
/// foreach ([3, 4, 5] as $p) foreach ([128, 160, 192, 224, 256] as $b) {
///     $a = "haval$b,$p"; $c = hash_init($a); hash_update($c, str_repeat('ab', 80)); hash_update($c, 'c');
///     echo $a, ' ', hash($a, 'abc'), ' ', hash_final($c), "\n";
/// }
/// ```
/// ```expect
/// haval128,3 9e40ed883fb63e985d299b40cda2b8f2 5b40a8b63a653f7e857cd19a89e359a1
/// haval160,3 b21e876c4d391e2a897661149d83576b5530a089 b437ff5e9c4b0bae73226a86cc1eca84f2de9996
/// haval192,3 a7b14c9ef3092319b0e75e3b20b957d180bf20745629e8de 92e3e0c43f1ffe0c52922c9dd5c5ba995644ef9ee06c01a3
/// haval224,3 5bc955220ba2346a948d2848eca37bdd5eca6ecca7b594bd32923fab 103bded69349f2b038cedcf9a8215e781a9109b4d09106e890cbee54
/// haval256,3 8699f1e3384d05b2a84b032693e2b6f46df85a13a50d93808d6874bb8fb9e86c 360c71411c8228df29da64ebd00ddb59a01f43028292f37011353d3f8ae7446d
/// haval128,4 6f2132867c9648419adcd5013e532fa2 d3c06b2927ac37506c4056ff5b460aec
/// haval160,4 77aca22f5b12cc09010afc9c0797308638b1cb9b 7a10b096f2b3838e8d654dd3a22b5689525012d2
/// haval192,4 7e29881ed05c915903dd5e24a8e81cde5d910142ae66207c 5aa5ddf552dac0e34fad6ebd0ed9fbb01d6835cd57e99a22
/// haval224,4 124c43d2ba4884599d013e8c872bfea4c88b0b6bf6303974cbe04e68 06b3d74544a8d5c31c2ab961aa11e2097df68863db38ca7404e0f6ea
/// haval256,4 8f409f1bb6b30c5016fdce55f652642261575bedca0b9533f32f5455459142b5 9fa97669e4debb007735c2753e2c4e395336715fe7c34a85fb46eaf841f54cee
/// haval128,5 d054232fe874d9c6c6dc8e6a853519ea 1ab82cad6fada499453a249eec1163ef
/// haval160,5 ae646b04845e3351f00c5161d138940e1fa0c11c a3101c028df8f062cd476b36ad7b27a052933c47
/// haval192,5 d12091104555b00119a8d07808a3380bf9e60018915b9025 2bd4a1697da0bb63d839084e8f4455ee3c8e0bd32127a9cf
/// haval224,5 8081027a500147c512e5f1055986674d746d92af4841abeb89da64ad d001d186278edd012b27857495add3a6dcbf5e2a32e9130d76a61759
/// haval256,5 976cd6254c337969e5913b158392a2921af16fca51f5601d486e0a9de01156e7 0d8efbbf78e20721f3334bc360b7cd448916a9b814a3858a63fe8d4a244dbee3
/// ```
#[derive(Clone)]
pub(crate) struct Haval<const P: usize, const B: usize> {
    state: [u32; 8],
    count: [u32; 2],
    buffer: [u8; 128],
}

impl<const P: usize, const B: usize> Default for Haval<P, B> {
    fn default() -> Haval<P, B> {
        let mut state = [0u32; 8];
        state.copy_from_slice(&PI[..8]);
        Haval {
            state,
            count: [0; 2],
            buffer: [0; 128],
        }
    }
}

impl<const P: usize, const B: usize> Haval<P, B> {
    /// `PHP_HAVAL<b>Final`'s fold of eight words down to `B` bits.
    fn fold(&mut self) {
        let s = &mut self.state;
        let [s4, s5, s6, s7] = [s[4], s[5], s[6], s[7]];
        match B {
            128 => {
                s[3] = s[3].wrapping_add(
                    (s7 & 0xFF00_0000) | (s6 & 0x00FF_0000) | (s5 & 0x0000_FF00) | (s4 & 0xFF),
                );
                s[2] = s[2].wrapping_add(
                    (((s7 & 0x00FF_0000) | (s6 & 0xFF00) | (s5 & 0xFF)) << 8)
                        | ((s4 & 0xFF00_0000) >> 24),
                );
                s[1] = s[1].wrapping_add(
                    (((s7 & 0xFF00) | (s6 & 0xFF)) << 16)
                        | (((s5 & 0xFF00_0000) | (s4 & 0x00FF_0000)) >> 16),
                );
                s[0] = s[0].wrapping_add(
                    ((s7 & 0xFF) << 24)
                        | (((s6 & 0xFF00_0000) | (s5 & 0x00FF_0000) | (s4 & 0xFF00)) >> 8),
                );
            }
            160 => {
                s[4] = s4.wrapping_add(
                    ((s7 & 0xFE00_0000) | (s6 & 0x01F8_0000) | (s5 & 0x0007_F000)) >> 12,
                );
                s[3] = s[3]
                    .wrapping_add(((s7 & 0x01F8_0000) | (s6 & 0x0007_F000) | (s5 & 0x0FC0)) >> 6);
                s[2] = s[2].wrapping_add((s7 & 0x0007_F000) | (s6 & 0x0FC0) | (s5 & 0x3F));
                s[1] = s[1].wrapping_add(
                    ((s7 & 0x0FC0) | (s6 & 0x3F) | (s5 & 0xFE00_0000)).rotate_right(25),
                );
                s[0] = s[0].wrapping_add(
                    ((s7 & 0x3F) | (s6 & 0xFE00_0000) | (s5 & 0x01F8_0000)).rotate_right(19),
                );
            }
            192 => {
                s[5] = s5.wrapping_add(((s7 & 0xFC00_0000) | (s6 & 0x03E0_0000)) >> 21);
                s[4] = s4.wrapping_add(((s7 & 0x03E0_0000) | (s6 & 0x001F_0000)) >> 16);
                s[3] = s[3].wrapping_add(((s7 & 0x001F_0000) | (s6 & 0xFC00)) >> 10);
                s[2] = s[2].wrapping_add(((s7 & 0xFC00) | (s6 & 0x03E0)) >> 5);
                s[1] = s[1].wrapping_add((s7 & 0x03E0) | (s6 & 0x1F));
                s[0] = s[0].wrapping_add(((s7 & 0x1F) | (s6 & 0xFC00_0000)).rotate_right(26));
            }
            224 => {
                s[6] = s6.wrapping_add(s7 & 0x0F);
                s[5] = s5.wrapping_add((s7 >> 4) & 0x1F);
                s[4] = s4.wrapping_add((s7 >> 9) & 0x0F);
                s[3] = s[3].wrapping_add((s7 >> 13) & 0x1F);
                s[2] = s[2].wrapping_add((s7 >> 18) & 0x0F);
                s[1] = s[1].wrapping_add((s7 >> 22) & 0x1F);
                s[0] = s[0].wrapping_add((s7 >> 27) & 0x1F);
            }
            _ => {}
        }
    }
}

impl<const P: usize, const B: usize> State for Haval<P, B> {
    /// `PHP_HAVALUpdate`.
    #[allow(clippy::cast_possible_truncation)]
    fn update(&mut self, input: &[u8]) {
        let index = ((self.count[0] >> 3) & 0x7f) as usize;
        let bits = (u64::from(self.count[1]) << 32 | u64::from(self.count[0]))
            .wrapping_add((input.len() as u64).wrapping_mul(8));
        self.count = [bits as u32, (bits >> 32) as u32];
        let state = &mut self.state;
        feed(&mut self.buffer, index, input, |block| {
            transform::<P>(state, block);
        });
    }

    /// `PHP_HAVAL<b>Final`: the padding starts with 1, then the version,
    /// passes and digest length, the bit count, and the fold.
    #[allow(clippy::cast_possible_truncation)]
    fn finish(&mut self) -> Vec<u8> {
        let mut tail = [0u8; 10];
        tail[0] = 1 | ((P as u8 & 7) << 3) | ((B as u8 & 3) << 6);
        tail[1] = (B >> 2) as u8;
        tail[2..6].copy_from_slice(&self.count[0].to_le_bytes());
        tail[6..].copy_from_slice(&self.count[1].to_le_bytes());
        let index = ((self.count[0] >> 3) & 0x7f) as usize;
        let pad = if index < 118 {
            118 - index
        } else {
            246 - index
        };
        let mut padding = [0u8; 128];
        padding[0] = 1;
        self.update(&padding[..pad]);
        self.update(&tail);
        self.fold();
        let mut out: Vec<u8> = self.state.iter().flat_map(|w| w.to_le_bytes()).collect();
        out.truncate(B / 8);
        out
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![
            Slot::L(&mut self.state),
            Slot::L(&mut self.count),
            Slot::B(&mut self.buffer),
        ]
    }
}
