//! `Z/ext/standard/net.c`: `net_get_interfaces` over the host's
//! `getifaddrs(3)`.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::decl::FunctionDecl;
use php_ext::raising::{Level, Thrown, raise};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value};

use crate::argument_count_between;
use crate::output::Entry;
use crate::stub::stub;

/// The one native.
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
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![(
        stub("net_get_interfaces(): array|false"),
        net_get_interfaces as Entry,
    )]
}

/// A string key of the request's.
fn key(req: &mut Request, name: &[u8]) -> Result<Key, Thrown> {
    Ok(Key::Str(PhpString::new(req, name)?))
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
fn net_get_interfaces(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    let found = match crate::ffi::interfaces() {
        Ok(found) => found,
        Err(error) => {
            let text = format!(
                "getifaddrs() failed {}: {}",
                error.raw_os_error().unwrap_or(0),
                crate::filestat::strerror(&error)
            );
            raise(req, Level::Warning, &text);
            return Ok(Value::bool(false));
        }
    };
    let mut names: Vec<(Vec<u8>, bool, PhpArray)> = Vec::new();
    for entry in found {
        let mut unicast_entry = PhpArray::new(req)?;
        let k = key(req, b"flags")?;
        unicast_entry.set(req, k, Value::int(i64::from(entry.flags)))?;
        if let Some(family) = entry.family {
            let k = key(req, b"family")?;
            unicast_entry.set(req, k, Value::int(family))?;
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
                let v = Value::string(PhpString::new(req, &text)?);
                unicast_entry.set(req, k, v)?;
            }
        }
        let up = entry.flags & IFF_UP != 0;
        if let Some(slot) = names.iter_mut().find(|(n, _, _)| *n == entry.name) {
            slot.2.append(req, Value::array(unicast_entry))?;
        } else {
            let mut unicast = PhpArray::new(req)?;
            unicast.append(req, Value::array(unicast_entry))?;
            names.push((entry.name, up, unicast));
        }
    }
    let mut array = PhpArray::new(req)?;
    for (name, up, unicast) in names {
        let mut iface = PhpArray::new(req)?;
        let k = key(req, b"unicast")?;
        iface.set(req, k, Value::array(unicast))?;
        let k = key(req, b"up")?;
        iface.set(req, k, Value::bool(up))?;
        let k = key(req, &name)?;
        array.set(req, k, Value::array(iface))?;
    }
    Ok(Value::array(array))
}

/// `IFF_UP`, `<net/if.h>`'s.
const IFF_UP: u32 = 1;
