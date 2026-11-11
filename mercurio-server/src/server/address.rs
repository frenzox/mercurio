//! Client addresses: the ranges that name them, and how many connections each
//! holds for the per-address limit.

use std::{collections::HashMap, net::IpAddr, sync::Arc};

/// Connections each client address holds, keyed by `address_key`.
#[derive(Default)]
pub(super) struct AddressSlots(std::sync::Mutex<HashMap<IpAddr, usize>>);

/// Whom `ip` counts as for the per-address limit. An IPv4 client of an
/// IPv6 socket counts as its IPv4 address. An IPv6 client counts as its
/// /64, the block one subscriber is usually given, so it can't take a new
/// address from it for every connection.
pub(crate) fn address_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6((u128::from(v6) & !u128::from(u64::MAX)).into()),
        },
        v4 => v4,
    }
}

impl AddressSlots {
    /// A slot for `ip`, unless its address or block holds `max` already.
    pub(super) fn acquire(self: &Arc<Self>, ip: IpAddr, max: usize) -> Option<AddressSlot> {
        let ip = address_key(ip);
        let mut counts = self.0.lock().unwrap();
        let count = counts.entry(ip).or_default();
        if *count >= max {
            return None;
        }
        *count += 1;
        Some(AddressSlot {
            slots: Arc::clone(self),
            ip,
        })
    }
}

/// One connection's place in its address's count, freed when dropped.
pub(super) struct AddressSlot {
    slots: Arc<AddressSlots>,
    ip: IpAddr,
}

impl Drop for AddressSlot {
    fn drop(&mut self) {
        let mut counts = self.slots.0.lock().unwrap();
        if let Some(count) = counts.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.ip);
            }
        }
    }
}

/// An address, or a block of them in CIDR form, such as `10.0.0.0/8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressRange {
    network: IpAddr,
    prefix: u8,
}

impl AddressRange {
    /// Whether `ip` is in the range. An IPv4 address mapped into IPv6, as
    /// IPv4 clients of an IPv6 socket appear, counts as the IPv4 address.
    pub fn contains(&self, ip: IpAddr) -> bool {
        // An IPv4 client may reach an IPv6 socket as a mapped address
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        match (self.network, ip) {
            (IpAddr::V4(network), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(network) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(network), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(network) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

impl std::str::FromStr for AddressRange {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, String> {
        let invalid = || format!("`{text}` is not an address or a CIDR block");
        let (address, prefix) = text.split_once('/').unwrap_or((text, ""));
        let network: IpAddr = address.parse().map_err(|_| invalid())?;
        let bits = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            "" => bits,
            prefix => prefix
                .parse()
                .ok()
                .filter(|p| *p <= bits)
                .ok_or_else(invalid)?,
        };
        // `contains` sees IPv4 clients as IPv4, so a mapped range has to be
        // one too to match anyone
        if let IpAddr::V6(v6) = network {
            if let Some(v4) = v6.to_ipv4_mapped() {
                if prefix < 96 {
                    return Err(format!(
                        "`{text}` is an IPv4-mapped block shorter than /96; write it as IPv4"
                    ));
                }
                return Ok(AddressRange {
                    network: IpAddr::V4(v4),
                    prefix: prefix - 96,
                });
            }
        }
        Ok(AddressRange { network, prefix })
    }
}

impl std::fmt::Display for AddressRange {
    /// The range's first address, then its length unless it holds one
    /// address: `10.0.0.0/8`, `192.0.2.7`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (first, bits) = match self.network {
            IpAddr::V4(ip) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                (IpAddr::V4((u32::from(ip) & mask).into()), 32)
            }
            IpAddr::V6(ip) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                (IpAddr::V6((u128::from(ip) & mask).into()), 128)
            }
        };
        if self.prefix == bits {
            write!(f, "{first}")
        } else {
            write!(f, "{first}/{}", self.prefix)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_address_slots_count_mapped_addresses_as_ipv4_and_ipv6_by_64() {
        let slots = Arc::new(AddressSlots::default());
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();

        let _v4 = slots.acquire(ip("192.0.2.7"), 1).unwrap();
        assert!(slots.acquire(ip("::ffff:192.0.2.7"), 1).is_none());
        assert!(slots.acquire(ip("192.0.2.8"), 1).is_some());

        let _v6 = slots.acquire(ip("2001:db8:0:1::1"), 1).unwrap();
        assert!(slots.acquire(ip("2001:db8:0:1:ffff::2"), 1).is_none());
        assert!(slots.acquire(ip("2001:db8:0:2::1"), 1).is_some());
    }

    #[test]
    fn test_address_ranges_print_their_first_address() {
        let print = |text: &str| text.parse::<AddressRange>().unwrap().to_string();
        assert_eq!(print("10.1.2.3/8"), "10.0.0.0/8");
        assert_eq!(print("192.0.2.7"), "192.0.2.7");
        assert_eq!(print("192.0.2.7/32"), "192.0.2.7");
        assert_eq!(print("2001:db8::1/32"), "2001:db8::/32");
        assert_eq!(print("0.0.0.0/0"), "0.0.0.0/0");
    }

    #[test]
    fn test_mapped_ranges_are_ipv4_ranges() {
        let parse = |text: &str| text.parse::<AddressRange>();
        assert_eq!(parse("::ffff:192.0.2.7").unwrap().to_string(), "192.0.2.7");
        assert_eq!(
            parse("::ffff:192.0.2.0/120").unwrap().to_string(),
            "192.0.2.0/24"
        );
        assert!(parse("::ffff:192.0.2.7/95").is_err());
    }

    #[test]
    fn test_address_ranges() {
        let range: AddressRange = "10.0.0.0/8".parse().unwrap();
        assert!(range.contains("10.200.0.1".parse().unwrap()));
        assert!(!range.contains("11.0.0.1".parse().unwrap()));
        // An IPv4 client on an IPv6 socket
        assert!(range.contains("::ffff:10.0.0.1".parse().unwrap()));

        let single: AddressRange = "192.0.2.7".parse().unwrap();
        assert!(single.contains("192.0.2.7".parse().unwrap()));
        assert!(!single.contains("192.0.2.8".parse().unwrap()));

        let v6: AddressRange = "2001:db8::/32".parse().unwrap();
        assert!(v6.contains("2001:db8:1::1".parse().unwrap()));
        assert!(!v6.contains("2001:db9::1".parse().unwrap()));

        let everything: AddressRange = "0.0.0.0/0".parse().unwrap();
        assert!(everything.contains("203.0.113.1".parse().unwrap()));

        for invalid in ["10.0.0.0/33", "example.com", "10.0.0.0/x", ""] {
            assert!(invalid.parse::<AddressRange>().is_err(), "{invalid}");
        }
    }
}
