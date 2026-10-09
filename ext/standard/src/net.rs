//! `Z/ext/standard/net.c`: `net_get_interfaces` over the host's
//! `getifaddrs(3)`.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::OrFalse;
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, raise};
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, PhpString};

/// A string key of the request's.
fn key(req: &mut Request, name: &[u8]) -> Result<Owner<Key>, Thrown> {
    Ok(PhpString::new(req, name)?.into_key())
}

/// `net_get_interfaces(): array|false`, `PHP_FUNCTION(net_get_interfaces)`
/// (`Z/ext/standard/net.c:117`): each interface by name with `unicast`,
/// one array per address in `getifaddrs(3)`'s order (its flags, family,
/// and the address, netmask, broadcast and point-to-point peer
/// `php_inet_ntop` can render), and `up` from the first entry's flags.
/// What the host's interfaces are is its own, so the witness pins the
/// loopback's first address and shape.
///
/// ```php
/// $lo = net_get_interfaces()['lo0'];
/// var_dump(array_key_first_of($lo), $lo['up'], $lo['unicast'][1]);
/// function array_key_first_of($a) { foreach ($a as $k => $v) { return $k; } }
/// ```
/// ```expect
/// string(7) "unicast"
/// bool(true)
/// array(4) {
///   ["flags"]=>
///   int(32841)
///   ["family"]=>
///   int(2)
///   ["address"]=>
///   string(9) "127.0.0.1"
///   ["netmask"]=>
///   string(9) "255.0.0.0"
/// }
/// ```
///
/// ```php
/// echo new ReflectionFunction('net_get_interfaces');
/// ```
/// ```expect
/// Function [ <internal:standard> function net_get_interfaces ] {
///
///   - Parameters [0] {
///   }
///   - Return [ array|false ]
/// }
/// ```
#[php_function]
fn net_get_interfaces(req: &mut Request) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let found = match crate::ffi::interfaces() {
        Ok(found) => found,
        Err(error) => {
            let text = format!(
                "getifaddrs() failed {}: {}",
                error.raw_os_error().unwrap_or(0),
                crate::filestat::strerror(&error)
            );
            raise(req, Level::Warning, &text);
            return Ok(OrFalse::False);
        }
    };
    let mut names: Vec<(Vec<u8>, bool, Owner<PhpArray>)> = Vec::new();
    for entry in found {
        let mut unicast_entry = PhpArray::new(req)?;
        let k = key(req, b"flags")?;
        unicast_entry.set(req, k, Owner::int(i64::from(entry.flags)))?;
        if let Some(family) = entry.family {
            let k = key(req, b"family")?;
            unicast_entry.set(req, k, Owner::int(family))?;
        }
        let texts: [(&[u8], Option<Vec<u8>>); 4] = [
            (b"address", entry.address),
            (b"netmask", entry.netmask),
            (b"broadcast", entry.broadcast),
            (b"ptp", entry.ptp),
        ];
        for (name, text) in texts {
            if let Some(text) = text {
                let k = key(req, name)?;
                let v = PhpString::new(req, &text)?.into_value();
                unicast_entry.set(req, k, v)?;
            }
        }
        let up = entry.flags & IFF_UP != 0;
        if let Some(slot) = names.iter_mut().find(|(n, _, _)| *n == entry.name) {
            slot.2.append(req, unicast_entry.into_value())?;
        } else {
            let mut unicast = PhpArray::new(req)?;
            unicast.append(req, unicast_entry.into_value())?;
            names.push((entry.name, up, unicast));
        }
    }
    let mut array = PhpArray::new(req)?;
    for (name, up, unicast) in names {
        let mut iface = PhpArray::new(req)?;
        let k = key(req, b"unicast")?;
        iface.set(req, k, unicast.into_value())?;
        let k = key(req, b"up")?;
        iface.set(req, k, Owner::bool(up))?;
        let k = key(req, &name)?;
        array.set(req, k, iface.into_value())?;
    }
    Ok(OrFalse::Value(array))
}

/// `IFF_UP`, `<net/if.h>`'s.
const IFF_UP: u32 = 1;
