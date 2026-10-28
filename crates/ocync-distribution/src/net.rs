//! Address rules shared by the requests that must keep a secret away from
//! a metadata service or a proxy.

use std::net::{Ipv4Addr, Ipv6Addr};

/// The EC2 instance metadata service's IPv6 address, which is unique-local
/// rather than link-local.
pub(crate) const EC2_METADATA_IPV6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254);

/// The IPv4 address `ip` carries, if any: IPv4-mapped (`::ffff:a.b.c.d`),
/// IPv4-translated (RFC 6145, `::ffff:0:a.b.c.d`) or the NAT64 well-known
/// prefix (RFC 6052, `64:ff9b::a.b.c.d`). For checks that block only: a
/// translated or NAT64 address goes through a translator off this host, so
/// a check that grants, such as "is local", reads only the mapped form.
pub(crate) fn embedded_ipv4(ip: &Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return Some(mapped);
    }
    let segs = ip.segments();
    let is_translated = segs[..6] == [0, 0, 0, 0, 0xffff, 0];
    let is_nat64 = segs[..6] == [0x0064, 0xff9b, 0, 0, 0, 0];
    (is_translated || is_nat64).then(|| {
        Ipv4Addr::new(
            (segs[6] >> 8) as u8,
            segs[6] as u8,
            (segs[7] >> 8) as u8,
            segs[7] as u8,
        )
    })
}
