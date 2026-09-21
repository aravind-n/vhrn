//! Reviewed IANA special-purpose address boundary.

use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistryFlag {
    True,
    False,
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ipv4Entry {
    network: Ipv4Addr,
    prefix: u8,
    destination: RegistryFlag,
    forwardable: RegistryFlag,
    globally_reachable: RegistryFlag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ipv6Entry {
    network: Ipv6Addr,
    prefix: u8,
    destination: RegistryFlag,
    forwardable: RegistryFlag,
    globally_reachable: RegistryFlag,
}

impl Ipv4Entry {
    const fn eligible(self) -> bool {
        three_true(self.destination, self.forwardable, self.globally_reachable)
    }
}

impl Ipv6Entry {
    const fn eligible(self) -> bool {
        three_true(self.destination, self.forwardable, self.globally_reachable)
    }
}

const fn three_true(
    destination: RegistryFlag,
    forwardable: RegistryFlag,
    globally_reachable: RegistryFlag,
) -> bool {
    matches!(destination, RegistryFlag::True)
        && matches!(forwardable, RegistryFlag::True)
        && matches!(globally_reachable, RegistryFlag::True)
}

const T: RegistryFlag = RegistryFlag::True;
const F: RegistryFlag = RegistryFlag::False;
const N: RegistryFlag = RegistryFlag::Indeterminate;

const fn v4(
    octets: [u8; 4],
    prefix: u8,
    destination: RegistryFlag,
    forwardable: RegistryFlag,
    globally_reachable: RegistryFlag,
) -> Ipv4Entry {
    Ipv4Entry {
        network: Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]),
        prefix,
        destination,
        forwardable,
        globally_reachable,
    }
}

const fn v6(
    segments: [u16; 8],
    prefix: u8,
    destination: RegistryFlag,
    forwardable: RegistryFlag,
    globally_reachable: RegistryFlag,
) -> Ipv6Entry {
    Ipv6Entry {
        network: Ipv6Addr::new(
            segments[0],
            segments[1],
            segments[2],
            segments[3],
            segments[4],
            segments[5],
            segments[6],
            segments[7],
        ),
        prefix,
        destination,
        forwardable,
        globally_reachable,
    }
}

// IANA IPv4 Special-Purpose Address Space, last updated 2025-10-09:
// https://www.iana.org/assignments/iana-ipv4-special-registry/
// A registry row containing two /32 blocks is represented by two entries.
const IPV4_SPECIAL_PURPOSE: &[Ipv4Entry] = &[
    v4([0, 0, 0, 0], 8, F, F, F),
    v4([0, 0, 0, 0], 32, F, F, F),
    v4([10, 0, 0, 0], 8, T, T, F),
    v4([100, 64, 0, 0], 10, T, T, F),
    v4([127, 0, 0, 0], 8, F, F, F),
    v4([169, 254, 0, 0], 16, T, F, F),
    v4([172, 16, 0, 0], 12, T, T, F),
    v4([192, 0, 0, 0], 24, F, F, F),
    v4([192, 0, 0, 0], 29, T, T, F),
    v4([192, 0, 0, 8], 32, F, F, F),
    v4([192, 0, 0, 9], 32, T, T, T),
    v4([192, 0, 0, 10], 32, T, T, T),
    v4([192, 0, 0, 170], 32, F, F, F),
    v4([192, 0, 0, 171], 32, F, F, F),
    v4([192, 0, 2, 0], 24, F, F, F),
    v4([192, 31, 196, 0], 24, T, T, T),
    v4([192, 52, 193, 0], 24, T, T, T),
    v4([192, 88, 99, 0], 24, N, N, N),
    v4([192, 88, 99, 2], 32, T, T, F),
    v4([192, 168, 0, 0], 16, T, T, F),
    v4([192, 175, 48, 0], 24, T, T, T),
    v4([198, 18, 0, 0], 15, T, T, F),
    v4([198, 51, 100, 0], 24, F, F, F),
    v4([203, 0, 113, 0], 24, F, F, F),
    v4([240, 0, 0, 0], 4, F, F, F),
    v4([255, 255, 255, 255], 32, T, F, F),
];

// IANA IPv6 Special-Purpose Address Space, last updated 2025-10-09:
// https://www.iana.org/assignments/iana-ipv6-special-registry/
const IPV6_SPECIAL_PURPOSE: &[Ipv6Entry] = &[
    v6([0, 0, 0, 0, 0, 0, 0, 1], 128, F, F, F),
    v6([0, 0, 0, 0, 0, 0, 0, 0], 128, F, F, F),
    v6([0, 0, 0, 0, 0, 0xffff, 0, 0], 96, F, F, F),
    v6([0x64, 0xff9b, 0, 0, 0, 0, 0, 0], 96, T, T, T),
    v6([0x64, 0xff9b, 1, 0, 0, 0, 0, 0], 48, T, T, F),
    v6([0x100, 0, 0, 0, 0, 0, 0, 0], 64, T, T, F),
    v6([0x100, 0, 0, 1, 0, 0, 0, 0], 64, T, F, F),
    v6([0x2001, 0, 0, 0, 0, 0, 0, 0], 23, F, F, F),
    v6([0x2001, 0, 0, 0, 0, 0, 0, 0], 32, T, T, N),
    v6([0x2001, 1, 0, 0, 0, 0, 0, 1], 128, T, T, T),
    v6([0x2001, 1, 0, 0, 0, 0, 0, 2], 128, T, T, T),
    v6([0x2001, 1, 0, 0, 0, 0, 0, 3], 128, T, T, T),
    v6([0x2001, 2, 0, 0, 0, 0, 0, 0], 48, T, T, F),
    v6([0x2001, 3, 0, 0, 0, 0, 0, 0], 32, T, T, T),
    v6([0x2001, 4, 0x112, 0, 0, 0, 0, 0], 48, T, T, T),
    v6([0x2001, 0x10, 0, 0, 0, 0, 0, 0], 28, N, N, N),
    v6([0x2001, 0x20, 0, 0, 0, 0, 0, 0], 28, T, T, T),
    v6([0x2001, 0x30, 0, 0, 0, 0, 0, 0], 28, T, T, T),
    v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0], 32, F, F, F),
    v6([0x2002, 0, 0, 0, 0, 0, 0, 0], 16, T, T, N),
    v6([0x2620, 0x4f, 0x8000, 0, 0, 0, 0, 0], 48, T, T, T),
    v6([0x3fff, 0, 0, 0, 0, 0, 0, 0], 20, F, F, F),
    v6([0x5f00, 0, 0, 0, 0, 0, 0, 0], 16, T, T, F),
    v6([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7, T, T, F),
    v6([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10, T, F, F),
];

pub(super) fn ipv4_is_global(address: Ipv4Addr) -> bool {
    if address.is_multicast() || address == Ipv4Addr::BROADCAST {
        return false;
    }
    most_specific_v4(address).is_none_or(Ipv4Entry::eligible)
}

pub(super) fn ipv6_is_global(address: Ipv6Addr) -> bool {
    if address.is_multicast() || address.to_ipv4_mapped().is_some() {
        return false;
    }
    let special = most_specific_v6(address);
    if ipv6_in_prefix(address, Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0), 3) {
        special.is_none_or(Ipv6Entry::eligible)
    } else {
        special.is_some_and(Ipv6Entry::eligible)
    }
}

fn most_specific_v4(address: Ipv4Addr) -> Option<Ipv4Entry> {
    IPV4_SPECIAL_PURPOSE
        .iter()
        .filter(|entry| ipv4_in_prefix(address, entry.network, entry.prefix))
        .max_by_key(|entry| entry.prefix)
        .copied()
}

fn most_specific_v6(address: Ipv6Addr) -> Option<Ipv6Entry> {
    IPV6_SPECIAL_PURPOSE
        .iter()
        .filter(|entry| ipv6_in_prefix(address, entry.network, entry.prefix))
        .max_by_key(|entry| entry.prefix)
        .copied()
}

fn ipv4_in_prefix(address: Ipv4Addr, network: Ipv4Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    u32::from(address) & mask == u32::from(network) & mask
}

fn ipv6_in_prefix(address: Ipv6Addr, network: Ipv6Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - u32::from(prefix))
    };
    u128::from(address) & mask == u128::from(network) & mask
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::net::IpAddr;

    use super::*;

    #[derive(Clone, Copy)]
    struct FixtureEntry {
        address: IpAddr,
        prefix: u8,
        destination: RegistryFlag,
        forwardable: RegistryFlag,
        globally_reachable: RegistryFlag,
    }

    #[test]
    fn runtime_registry_is_the_complete_reviewed_fixture() {
        let fixture = fixture_entries();
        let fixture_v4: Vec<_> = fixture
            .iter()
            .filter_map(|entry| match entry.address {
                IpAddr::V4(network) => Some(Ipv4Entry {
                    network,
                    prefix: entry.prefix,
                    destination: entry.destination,
                    forwardable: entry.forwardable,
                    globally_reachable: entry.globally_reachable,
                }),
                IpAddr::V6(_) => None,
            })
            .collect();
        let fixture_v6: Vec<_> = fixture
            .iter()
            .filter_map(|entry| match entry.address {
                IpAddr::V6(network) => Some(Ipv6Entry {
                    network,
                    prefix: entry.prefix,
                    destination: entry.destination,
                    forwardable: entry.forwardable,
                    globally_reachable: entry.globally_reachable,
                }),
                IpAddr::V4(_) => None,
            })
            .collect();

        assert_eq!(IPV4_SPECIAL_PURPOSE, fixture_v4);
        assert_eq!(IPV6_SPECIAL_PURPOSE, fixture_v6);
    }

    #[test]
    fn every_registry_prefix_boundary_matches_the_reviewed_fixture() {
        let fixture = fixture_entries();
        let mut checked = HashSet::new();
        for entry in &fixture {
            match entry.address {
                IpAddr::V4(network) => {
                    let network = u32::from(network);
                    let host_mask = if entry.prefix == 32 {
                        0
                    } else {
                        u32::MAX >> entry.prefix
                    };
                    for value in [
                        network.saturating_sub(1),
                        network,
                        network | host_mask,
                        (network | host_mask).saturating_add(1),
                    ] {
                        if checked.insert(IpAddr::V4(Ipv4Addr::from(value))) {
                            let address = Ipv4Addr::from(value);
                            assert_eq!(
                                ipv4_is_global(address),
                                fixture_v4_is_global(address, &fixture),
                                "{address} around {}/{}",
                                entry.address,
                                entry.prefix
                            );
                        }
                    }
                }
                IpAddr::V6(network) => {
                    let network = u128::from(network);
                    let host_mask = if entry.prefix == 128 {
                        0
                    } else {
                        u128::MAX >> entry.prefix
                    };
                    for value in [
                        network.saturating_sub(1),
                        network,
                        network | host_mask,
                        (network | host_mask).saturating_add(1),
                    ] {
                        if checked.insert(IpAddr::V6(Ipv6Addr::from(value))) {
                            let address = Ipv6Addr::from(value);
                            assert_eq!(
                                ipv6_is_global(address),
                                fixture_v6_is_global(address, &fixture),
                                "{address} around {}/{}",
                                entry.address,
                                entry.prefix
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn non_registry_unicast_and_explicit_non_unicast_follow_the_outer_boundary() {
        for (address, expected) in [
            ("8.8.8.8", true),
            ("224.0.0.0", false),
            ("239.255.255.255", false),
            ("255.255.255.255", false),
            ("2606:4700:4700::1111", true),
            ("4000::1", false),
            ("ff00::1", false),
            ("::ffff:8.8.8.8", false),
        ] {
            let address: IpAddr = address.parse().unwrap();
            let actual = match address {
                IpAddr::V4(address) => ipv4_is_global(address),
                IpAddr::V6(address) => ipv6_is_global(address),
            };
            assert_eq!(actual, expected, "{address}");
        }
    }

    fn fixture_entries() -> Vec<FixtureEntry> {
        include_str!("../../../testdata/iana-special-purpose-addresses.tsv")
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let fields: Vec<_> = line.split('\t').collect();
                let [prefix, destination, forwardable, globally_reachable] = fields.as_slice()
                else {
                    panic!("bad registry fixture row: {line}");
                };
                let (address, prefix) = prefix.split_once('/').unwrap();
                FixtureEntry {
                    address: address.parse().unwrap(),
                    prefix: prefix.parse().unwrap(),
                    destination: flag(destination),
                    forwardable: flag(forwardable),
                    globally_reachable: flag(globally_reachable),
                }
            })
            .collect()
    }

    fn flag(value: &str) -> RegistryFlag {
        match value {
            "T" => T,
            "F" => F,
            "N" => N,
            _ => panic!("bad registry flag: {value}"),
        }
    }

    fn fixture_v4_is_global(address: Ipv4Addr, entries: &[FixtureEntry]) -> bool {
        if address.is_multicast() || address == Ipv4Addr::BROADCAST {
            return false;
        }
        entries
            .iter()
            .filter(|entry| match entry.address {
                IpAddr::V4(network) => ipv4_in_prefix(address, network, entry.prefix),
                IpAddr::V6(_) => false,
            })
            .max_by_key(|entry| entry.prefix)
            .is_none_or(fixture_eligible)
    }

    fn fixture_v6_is_global(address: Ipv6Addr, entries: &[FixtureEntry]) -> bool {
        if address.is_multicast() || address.to_ipv4_mapped().is_some() {
            return false;
        }
        let special = entries
            .iter()
            .filter(|entry| match entry.address {
                IpAddr::V6(network) => ipv6_in_prefix(address, network, entry.prefix),
                IpAddr::V4(_) => false,
            })
            .max_by_key(|entry| entry.prefix);
        if ipv6_in_prefix(address, "2000::".parse().unwrap(), 3) {
            special.is_none_or(fixture_eligible)
        } else {
            special.is_some_and(fixture_eligible)
        }
    }

    fn fixture_eligible(entry: &FixtureEntry) -> bool {
        three_true(
            entry.destination,
            entry.forwardable,
            entry.globally_reachable,
        )
    }
}
