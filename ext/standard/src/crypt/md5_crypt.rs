// SPDX-License-Identifier: MIT AND BSD-3-Clause AND Beerware
// Copyright © The PHP Group and Contributors.
// Copyright © Zend Technologies Ltd., a subsidiary company of Perforce Software, Inc.
// Derived in part from php-src, used under BSD-3-Clause: see NOTICE.
// A port of md5crypt as php-src carries it, an altered version: see THIRD-PARTY.md.
//
// md5crypt's notice, as php-src carries it:
//   /*
//    * License for the Unix md5crypt implementation (md5_crypt):
//    *
//    * ----------------------------------------------------------------------------
//    * "THE BEER-WARE LICENSE" (Revision 42):
//    * <phk@login.dknet.dk> wrote this file.  As long as you retain this notice you
//    * can do whatever you want with this stuff. If we meet some day, and you think
//    * this stuff is worth it, you can buy me a beer in return.   Poul-Henning Kamp
//    * ----------------------------------------------------------------------------
//    *
//    * from FreeBSD: crypt.c,v 1.5 1996/10/14 08:34:02 phk Exp
//    * via OpenBSD: md5crypt.c,v 1.9 1997/07/23 20:58:27 kstailey Exp
//    * via NetBSD: md5crypt.c,v 1.4.2.1 2002/01/22 19:31:59 he Exp
//    *
//    */
// It stands, byte for byte, in LICENSES/md5crypt.txt.

//! The `$1$` hash, php's MD5-based crypt (`Z/ext/standard/php_crypt_r.c:101`).

use crate::md5::Md5;

/// `MD5_MAGIC`, `Z/ext/standard/php_crypt_r.c:83`.
const MD5_MAGIC: &[u8] = b"$1$";

/// `itoa64`, `Z/ext/standard/php_crypt_r.c:86`.
const ITOA64: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// `to64`, `Z/ext/standard/php_crypt_r.c:90`: `n` base-64 digits of `v`, low
/// digit first.
fn to64(out: &mut Vec<u8>, mut v: u32, n: usize) {
    for _ in 0..n {
        out.push(ITOA64[usize::from(v.to_le_bytes()[0] & 0x3f)]);
        v >>= 6;
    }
}

/// `php_md5_crypt_r`, `Z/ext/standard/php_crypt_r.c:101`. `pw` and `salt` are
/// the C strings php passes: each ends before its first NUL. Never fails.
pub(super) fn php_md5_crypt_r(pw: &[u8], salt: &[u8]) -> Vec<u8> {
    // If it starts with the magic string, then skip that.
    let sp = salt.strip_prefix(MD5_MAGIC).unwrap_or(salt);
    // It stops at the first '$', at most 8 chars.
    let sl = sp.iter().take(8).take_while(|&&c| c != b'$').count();
    let sp = sp.get(..sl).unwrap_or(sp);

    let mut ctx = Md5::new();
    ctx.update(pw);
    ctx.update(MD5_MAGIC);
    ctx.update(sp);

    let mut ctx1 = Md5::new();
    ctx1.update(pw);
    ctx1.update(sp);
    ctx1.update(pw);
    let mut fin = ctx1.finish();

    let mut pl = pw.len();
    while pl > 0 {
        let n = pl.min(16);
        ctx.update(fin.get(..n).unwrap_or(&fin));
        pl -= n;
    }

    // php zeroes `final` here, so the loop below feeds a zero byte.
    fin = [0; 16];

    // Then something really weird...
    let mut i = pw.len();
    while i != 0 {
        if i & 1 != 0 {
            ctx.update(&fin[..1]);
        } else {
            ctx.update(pw.get(..1).unwrap_or(&[]));
        }
        i >>= 1;
    }

    let mut passwd = Vec::with_capacity(MD5_MAGIC.len() + sl + 1 + 22);
    passwd.extend_from_slice(MD5_MAGIC);
    passwd.extend_from_slice(sp);
    passwd.push(b'$');

    fin = ctx.finish();

    for i in 0..1000u32 {
        let mut ctx1 = Md5::new();
        if i & 1 != 0 {
            ctx1.update(pw);
        } else {
            ctx1.update(&fin);
        }
        if i % 3 != 0 {
            ctx1.update(sp);
        }
        if i % 7 != 0 {
            ctx1.update(pw);
        }
        if i & 1 != 0 {
            ctx1.update(&fin);
        } else {
            ctx1.update(pw);
        }
        fin = ctx1.finish();
    }

    let f = fin.map(u32::from);
    to64(&mut passwd, (f[0] << 16) | (f[6] << 8) | f[12], 4);
    to64(&mut passwd, (f[1] << 16) | (f[7] << 8) | f[13], 4);
    to64(&mut passwd, (f[2] << 16) | (f[8] << 8) | f[14], 4);
    to64(&mut passwd, (f[3] << 16) | (f[9] << 8) | f[15], 4);
    to64(&mut passwd, (f[4] << 16) | (f[10] << 8) | f[5], 4);
    to64(&mut passwd, f[11], 2);
    passwd
}
