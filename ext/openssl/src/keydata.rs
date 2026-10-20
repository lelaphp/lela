//! The keys `openssl_pkey_new` makes from their parameters rather than
//! from the configuration, `openssl_backend_v3.c`'s
//! `php_openssl_pkey_init_rsa`, `_dsa`, `_dh`, `_ec` and
//! `php_openssl_pkey_object_curve_25519_448`: each an `$options` entry
//! (`rsa`, `dsa`, ...) whose members are the key's numbers as big-endian
//! strings, built into an `OSSL_PARAM` set and made with
//! `EVP_PKEY_fromdata`, or generated over the parameters where the key
//! itself is not given. Each stores what the library queued, as php's
//! `cleanup:` does.

use std::ffi::CStr;

use php_ext::raising::{Level, docref};
use php_ext::request::Request;
use php_ext::values::{PhpArray, ValueRef};

use crate::decode::c_string;
use crate::ffi::key::{Bn, EcGroup, EcPoint, KeyContext, ParamSet, Params};
use crate::ffi::{self, KeyKind, PKey};
use crate::store_errors;

/// `NID_sm2`, the curve whose custom parameters php skips.
const NID_SM2: i32 = 1172;

/// A key made, and whether it holds a private half.
pub(crate) type Made = Option<(PKey, bool)>;

/// `OPENSSL_PKEY_SET_BN(data, name)`: the member as a number where it is
/// a string, no number otherwise.
fn number(data: PhpArray, name: &[u8]) -> Option<Bn> {
    match data.find(name)?.as_ref() {
        ValueRef::Str(s) => Bn::from_bytes(s.as_bytes()),
        _ => None,
    }
}

/// A member that is a non-empty string, as its bytes.
fn octets(data: PhpArray, name: &[u8]) -> Option<Vec<u8>> {
    match data.find(name)?.as_ref() {
        ValueRef::Str(s) if !s.is_empty() => Some(s.as_bytes().to_vec()),
        _ => None,
    }
}

/// `EVP_PKEY_fromdata` of a built set in a new `EVP_PKEY_CTX` of the kind.
fn from_data(kind: KeyKind, params: Option<ParamSet>) -> Option<PKey> {
    KeyContext::of_kind(kind)?.fromdata(&params?)
}

/// A key generated over the parameters `params` holds.
fn generate_over(params: &PKey) -> Option<PKey> {
    let mut ctx = KeyContext::of_params(params)?;
    if ctx.keygen_init() <= 0 {
        return None;
    }
    ctx.keygen(false)
}

/// `php_openssl_pkey_init_rsa`: `n` and `d`, and each of `e`, `p`, `q`,
/// `dmp1`, `dmq1` and `iqmp` given.
pub(crate) fn rsa(req: &mut Request, data: PhpArray) -> Option<PKey> {
    let names: [(&[u8], &'static CStr); 8] = [
        (b"n", c"n"),
        (b"e", c"e"),
        (b"d", c"d"),
        (b"p", c"rsa-factor1"),
        (b"q", c"rsa-factor2"),
        (b"dmp1", c"rsa-exponent1"),
        (b"dmq1", c"rsa-exponent2"),
        (b"iqmp", c"rsa-coefficient1"),
    ];
    let mut numbers = Vec::new();
    for (member, name) in names {
        numbers.push((name, number(data, member)));
    }
    let key = (|| {
        let _ctx = KeyContext::of_kind(KeyKind::Rsa)?;
        let mut params = Params::new()?;
        let [n, e, d, rest @ ..] = &numbers[..] else {
            return None;
        };
        n.1.as_ref()?;
        d.1.as_ref()?;
        for (name, bn) in [n, d, e].into_iter().chain(rest) {
            if let Some(bn) = bn {
                params.push_bn(name, Some(bn));
            }
        }
        from_data(KeyKind::Rsa, params.build())
    })();
    store_errors(req);
    key
}

/// `php_openssl_pkey_init_dsa` and `php_openssl_pkey_init_dh`: `p` and
/// `g` (and `q`, which DSA requires), with `pub_key` and `priv_key`
/// making the key itself and their absence a key generated over the
/// parameters. DH derives a missing `pub_key` from `priv_key`; DSA takes
/// `priv_key` only beside `pub_key`.
pub(crate) fn ffc(req: &mut Request, kind: KeyKind, data: PhpArray) -> Made {
    let p = number(data, b"p");
    let q = number(data, b"q");
    let g = number(data, b"g");
    let private = number(data, b"priv_key");
    let mut public = number(data, b"pub_key");
    let dsa = kind == KeyKind::Dsa;
    let made = (|| {
        let _ctx = KeyContext::of_kind(kind)?;
        let (p, g) = (p.as_ref()?, g.as_ref()?);
        if dsa {
            q.as_ref()?;
        }
        if !dsa
            && public.is_none()
            && let Some(private) = &private
        {
            public = Some(private.dh_public(g, p)?);
        }
        let mut params = Params::new()?;
        params.push_bn(c"p", Some(p));
        if dsa {
            params.push_bn(c"q", q.as_ref());
        }
        params.push_bn(c"g", Some(g));
        if !dsa && let Some(q) = &q {
            params.push_bn(c"q", Some(q));
        }
        let takes_private = if dsa { public.is_some() } else { true };
        if dsa && let Some(public) = &public {
            params.push_bn(c"pub", Some(public));
        }
        if takes_private && let Some(private) = &private {
            params.push_bn(c"priv", Some(private));
        }
        if !dsa && let Some(public) = &public {
            params.push_bn(c"pub", Some(public));
        }
        let params_key = from_data(kind, params.build())?;
        let given = if dsa {
            public.is_some()
        } else {
            public.is_some() || private.is_some()
        };
        if given {
            return Some((params_key, private.is_some()));
        }
        generate_over(&params_key).map(|key| (key, true))
    })();
    store_errors(req);
    made
}

/// The members of an `ec` entry: a curve's name, or a prime curve's
/// numbers, generator and seed; and the key's `d`, or `x` and `y`.
struct EcData {
    curve: Option<Vec<u8>>,
    prime: Option<Bn>,
    coeff_a: Option<Bn>,
    coeff_b: Option<Bn>,
    order: Option<Bn>,
    g_x: Option<Bn>,
    g_y: Option<Bn>,
    cofactor: Option<Bn>,
    generator: Option<Vec<u8>>,
    seed: Option<Vec<u8>>,
    private: Option<Bn>,
    pub_x: Option<Bn>,
    pub_y: Option<Bn>,
}

impl EcData {
    fn read(data: PhpArray) -> EcData {
        EcData {
            curve: octets(data, b"curve_name"),
            prime: number(data, b"p"),
            coeff_a: number(data, b"a"),
            coeff_b: number(data, b"b"),
            order: number(data, b"order"),
            g_x: number(data, b"g_x"),
            g_y: number(data, b"g_y"),
            cofactor: number(data, b"cofactor"),
            generator: octets(data, b"generator"),
            seed: octets(data, b"seed"),
            private: number(data, b"d"),
            pub_x: number(data, b"x"),
            pub_y: number(data, b"y"),
        }
    }

    /// `d`, or `x` and `y`: the key itself is given.
    fn gives_key(&self) -> bool {
        self.private.is_some() || (self.pub_x.is_some() && self.pub_y.is_some())
    }
}

/// The group a named curve is, its name pushed as the group's; `None`
/// after php's warning for a name the library does not know.
fn named_group<'a>(
    params: &mut Params<'a>,
    name: &'a [u8],
    warning: &mut Option<String>,
) -> Option<(EcGroup, i32)> {
    let nid = ffi::short_name_nid(&c_string(name));
    if nid == 0 {
        *warning = Some(format!(
            "Unknown elliptic curve (short) name {}",
            String::from_utf8_lossy(c_string(name).as_bytes())
        ));
        return None;
    }
    let group = EcGroup::by_nid(nid)?;
    params.push_utf8(c"group", name).then_some((group, nid))
}

/// A prime curve of `p`, `a`, `b` and `order`, its generator encoded or
/// as `g_x` and `g_y` (compressed into `g_buf`), its seed and cofactor,
/// each pushed as the curve's; and the named curve it is, 0 for none.
/// `None` after php's warning for a missing member.
fn prime_group<'a>(
    params: &mut Params<'a>,
    ec: &'a EcData,
    g_buf: &'a mut Option<Vec<u8>>,
    warning: &mut Option<String>,
) -> Option<(EcGroup, i32)> {
    let (Some(prime), Some(coeff_a), Some(coeff_b), Some(order)) =
        (&ec.prime, &ec.coeff_a, &ec.coeff_b, &ec.order)
    else {
        let none = ec.prime.is_none()
            && ec.coeff_a.is_none()
            && ec.coeff_b.is_none()
            && ec.order.is_none();
        *warning = Some(if none {
            "Missing params: curve_name".to_owned()
        } else {
            "Missing params: curve_name or p, a, b, order".to_owned()
        });
        return None;
    };
    let pushed = params.push_bn(c"p", Some(prime))
        && params.push_bn(c"a", Some(coeff_a))
        && params.push_bn(c"b", Some(coeff_b))
        && params.push_bn(c"order", Some(order))
        && params.push_utf8(c"field-type", b"prime-field");
    if !pushed {
        return None;
    }
    let mut group = EcGroup::prime(prime, coeff_a, coeff_b)?;
    let mut generator = EcPoint::new(&group)?;
    if let Some(bytes) = &ec.generator {
        if !generator.set_octets(&group, bytes) || !params.push_octets(c"generator", bytes) {
            return None;
        }
    } else {
        let (Some(g_x), Some(g_y)) = (&ec.g_x, &ec.g_y) else {
            *warning = Some("Missing params: generator or g_x and g_y".to_owned());
            return None;
        };
        if !generator.set_affine(&group, g_x, g_y) {
            return None;
        }
        *g_buf = Some(generator.compressed(&group)?);
        let g_buf: &'a Option<Vec<u8>> = g_buf;
        if !params.push_octets(c"generator", g_buf.as_deref()?) {
            return None;
        }
    }
    if let Some(bytes) = &ec.seed
        && (!group.set_seed(bytes) || !params.push_octets(c"seed", bytes))
    {
        return None;
    }
    if !params.push_bn(c"cofactor", ec.cofactor.as_ref())
        || !group.set_generator(&generator, order, ec.cofactor.as_ref())
    {
        return None;
    }
    let nid = group.named_curve();
    Some((group, nid))
}

/// The public point `d` makes, or `x` and `y` are, compressed into
/// `q_buf` and pushed as the key's, with `d` as its private half;
/// nothing where neither is given. False where a step failed.
fn key_point<'a>(
    params: &mut Params<'a>,
    ec: &'a EcData,
    group: &EcGroup,
    q_buf: &'a mut Option<Vec<u8>>,
) -> bool {
    let point = if let Some(private) = &ec.private {
        let Some(mut point) = EcPoint::new(group) else {
            return false;
        };
        if !point.set_multiple(group, private) || !params.push_bn(c"priv", Some(private)) {
            return false;
        }
        point
    } else if let (Some(x), Some(y)) = (&ec.pub_x, &ec.pub_y) {
        let Some(mut point) = EcPoint::new(group) else {
            return false;
        };
        if !point.set_affine(group, x, y) {
            return false;
        }
        point
    } else {
        return true;
    };
    let Some(bytes) = point.compressed(group) else {
        return false;
    };
    *q_buf = Some(bytes);
    let q_buf: &'a Option<Vec<u8>> = q_buf;
    q_buf
        .as_deref()
        .is_some_and(|bytes| params.push_octets(c"pub", bytes))
}

/// The key the members make, or one generated on their curve.
fn build_ec(
    ec: &EcData,
    g_buf: &mut Option<Vec<u8>>,
    q_buf: &mut Option<Vec<u8>>,
    warning: &mut Option<String>,
) -> Made {
    let _ctx = KeyContext::of_kind(KeyKind::Ec)?;
    let mut params = Params::new()?;
    let (group, nid) = match &ec.curve {
        Some(name) => named_group(&mut params, name, warning)?,
        None => prime_group(&mut params, ec, g_buf, warning)?,
    };
    if nid != NID_SM2 && !key_point(&mut params, ec, &group, q_buf) {
        return None;
    }
    let params = params.build()?;
    let (mut ctx, key) = if ec.gives_key() {
        let key = KeyContext::of_kind(KeyKind::Ec)?.fromdata(&params)?;
        (KeyContext::of_key(&key)?, Some(key))
    } else {
        (KeyContext::of_kind(KeyKind::Ec)?, None)
    };
    if ctx.checks() {
        return key.map(|key| (key, ec.private.is_some()));
    }
    if ctx.keygen_init() != 1 || !ctx.set_params(&params) {
        return None;
    }
    ctx.keygen(true).map(|key| (key, true))
}

/// `php_openssl_pkey_init_ec`: a named curve, or a prime curve's `p`,
/// `a`, `b`, `order`, generator (encoded or as `g_x` and `g_y`), `seed`
/// and `cofactor`; then `d`, or `x` and `y`, making the key itself, and
/// their absence a key generated on the curve.
pub(crate) fn ec(req: &mut Request, data: PhpArray) -> Made {
    let ec = EcData::read(data);
    let mut warning = None;
    let (mut g_buf, mut q_buf) = (None, None);
    let made = build_ec(&ec, &mut g_buf, &mut q_buf, &mut warning);
    if let Some(text) = warning {
        docref(req, Level::Warning, &text);
    }
    store_errors(req);
    made
}

/// `php_openssl_pkey_object_curve_25519_448`: `priv_key` and `pub_key`
/// making the key where either member is there at all, and a key
/// generated where neither is.
pub(crate) fn curve(req: &mut Request, kind: KeyKind, data: PhpArray) -> Made {
    let private = octets(data, b"priv_key");
    let public = octets(data, b"pub_key");
    let has_private = data.find(b"priv_key").is_some();
    let has_public = data.find(b"pub_key").is_some();
    let made = (|| {
        let mut params = Params::new()?;
        if let Some(bytes) = &private
            && !params.push_octets(c"priv", bytes)
        {
            return None;
        }
        if let Some(bytes) = &public
            && !params.push_octets(c"pub", bytes)
        {
            return None;
        }
        let params = params.build();
        let mut ctx = KeyContext::of_kind(kind)?;
        let params = params?;
        if has_private || has_public {
            return ctx.fromdata(&params).map(|key| (key, has_private));
        }
        if ctx.keygen_init() <= 0 {
            return None;
        }
        ctx.keygen(false).map(|key| (key, true))
    })();
    store_errors(req);
    made
}

/// The array under one of `openssl_pkey_new`'s key-kind entries, the
/// first that is an array in php's order of them.
pub(crate) fn entry(options: PhpArray) -> Option<(KeyKind, PhpArray)> {
    let kinds: [(&[u8], KeyKind); 8] = [
        (b"rsa", KeyKind::Rsa),
        (b"dsa", KeyKind::Dsa),
        (b"dh", KeyKind::Dh),
        (b"ec", KeyKind::Ec),
        (b"x25519", KeyKind::X25519),
        (b"ed25519", KeyKind::Ed25519),
        (b"x448", KeyKind::X448),
        (b"ed448", KeyKind::Ed448),
    ];
    for (name, kind) in kinds {
        if let Some(value) = options.find(name)
            && let ValueRef::Array(data) = value.as_ref()
        {
            return Some((kind, data));
        }
    }
    None
}
