//! Python's `ipaddress` classifications (as of Python 3.13), which atif-scan's network
//! checks rely on. Rust's `is_global` is unstable and its private ranges differ.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// `ipaddress.ip_address(value)`, for strings.
pub fn parse(value: &str) -> Option<IpAddr> {
    value.parse().ok()
}

fn v4_in(ip: Ipv4Addr, net: [u8; 4], prefix: u32) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    (u32::from(ip) & mask) == (u32::from(Ipv4Addr::from(net)) & mask)
}

fn v6_in(ip: Ipv6Addr, net: [u16; 8], prefix: u32) -> bool {
    let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix) };
    (u128::from(ip) & mask) == (u128::from(Ipv6Addr::from(net)) & mask)
}

const V4_PRIVATE: &[([u8; 4], u32)] = &[
    ([0, 0, 0, 0], 8),
    ([10, 0, 0, 0], 8),
    ([127, 0, 0, 0], 8),
    ([169, 254, 0, 0], 16),
    ([172, 16, 0, 0], 12),
    ([192, 0, 0, 0], 24),
    ([192, 0, 2, 0], 24),
    ([192, 168, 0, 0], 16),
    ([198, 18, 0, 0], 15),
    ([198, 51, 100, 0], 24),
    ([203, 0, 113, 0], 24),
    ([240, 0, 0, 0], 4),
    ([255, 255, 255, 255], 32),
];
const V4_PRIVATE_EXCEPTIONS: &[([u8; 4], u32)] = &[([192, 0, 0, 9], 32), ([192, 0, 0, 10], 32)];

const V6_PRIVATE: &[([u16; 8], u32)] = &[
    ([0, 0, 0, 0, 0, 0, 0, 1], 128),
    ([0, 0, 0, 0, 0, 0, 0, 0], 128),
    ([0, 0, 0, 0, 0, 0xffff, 0, 0], 96),
    ([0x64, 0xff9b, 1, 0, 0, 0, 0, 0], 48),
    ([0x100, 0, 0, 0, 0, 0, 0, 0], 64),
    ([0x2001, 0, 0, 0, 0, 0, 0, 0], 23),
    ([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0], 32),
    ([0x2002, 0, 0, 0, 0, 0, 0, 0], 16),
    ([0x3fff, 0, 0, 0, 0, 0, 0, 0], 20),
    ([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7),
    ([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10),
];
const V6_PRIVATE_EXCEPTIONS: &[([u16; 8], u32)] = &[
    ([0x2001, 1, 0, 0, 0, 0, 0, 1], 128),
    ([0x2001, 1, 0, 0, 0, 0, 0, 2], 128),
    ([0x2001, 3, 0, 0, 0, 0, 0, 0], 32),
    ([0x2001, 4, 0x112, 0, 0, 0, 0, 0], 48),
    ([0x2001, 0x20, 0, 0, 0, 0, 0, 0], 28),
    ([0x2001, 0x30, 0, 0, 0, 0, 0, 0], 28),
];

fn mapped(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    (s[..5] == [0, 0, 0, 0, 0] && s[5] == 0xffff).then(|| Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8))
}

pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => V4_PRIVATE.iter().any(|(n, p)| v4_in(v4, *n, *p)) && !V4_PRIVATE_EXCEPTIONS.iter().any(|(n, p)| v4_in(v4, *n, *p)),
        IpAddr::V6(v6) => match mapped(v6) {
            Some(v4) => is_private(IpAddr::V4(v4)),
            None => V6_PRIVATE.iter().any(|(n, p)| v6_in(v6, *n, *p)) && !V6_PRIVATE_EXCEPTIONS.iter().any(|(n, p)| v6_in(v6, *n, *p)),
        },
    }
}

pub fn is_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4_in(v4, [100, 64, 0, 0], 10) && !is_private(ip),
        IpAddr::V6(v6) => match mapped(v6) {
            Some(v4) => is_global(IpAddr::V4(v4)),
            None => !is_private(ip),
        },
    }
}

pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] == 127,
        IpAddr::V6(v6) => match mapped(v6) {
            Some(v4) => v4.octets()[0] == 127,
            None => v6 == Ipv6Addr::LOCALHOST,
        },
    }
}

pub fn is_unspecified(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_unspecified(),
        IpAddr::V6(v6) => match mapped(v6) {
            Some(v4) => v4.is_unspecified(),
            None => v6.is_unspecified(),
        },
    }
}

/// `ip_address(value).is_global`, false when it isn't an address.
pub fn public(value: Option<&str>) -> bool {
    parse(value.unwrap_or("")).is_some_and(is_global)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes() {
        let g = |s: &str| is_global(parse(s).unwrap());
        assert!(g("8.8.8.8") && g("140.82.112.3") && g("2606:4700::1111"));
        assert!(!g("10.1.2.3") && !g("127.0.0.1") && !g("169.254.169.254") && !g("100.64.1.1") && !g("::1") && !g("fd00::1"));
        assert!(g("192.0.0.9"));
        assert!(!g("::ffff:10.0.0.1") && g("::ffff:8.8.8.8"));
        assert!(parse("01.2.3.4").is_none());
    }
}
