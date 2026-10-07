//! This machine's network addresses as the hub cares about them: those on
//! tunnel interfaces (where Tailscale lives — `utun*` on macOS,
//! `tailscale*` on Linux, per `HubConfig::tunnel_interfaces`), and the
//! tailnet IPv4 addresses to print in links.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Every interface address as (interface name, address).
fn interface_addresses() -> Vec<(String, IpAddr)> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, freed below; entries are read only
    // while the list is alive.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return Vec::new();
        }
        let mut found = Vec::new();
        let mut cursor = list;
        while !cursor.is_null() {
            let entry = &*cursor;
            cursor = entry.ifa_next;
            if entry.ifa_addr.is_null() || entry.ifa_name.is_null() {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(entry.ifa_name)
                .to_string_lossy()
                .into_owned();
            match i32::from((*entry.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let sin = &*(entry.ifa_addr as *const libc::sockaddr_in);
                    let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                    found.push((name, IpAddr::V4(ip)));
                }
                libc::AF_INET6 => {
                    let sin6 = &*(entry.ifa_addr as *const libc::sockaddr_in6);
                    found.push((name, IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr))));
                }
                _ => {}
            }
        }
        libc::freeifaddrs(list);
        found
    }
}

/// The addresses held by interfaces whose names start with one of
/// `prefixes` — the tunnel list `web::security::is_allowed_pair` checks
/// the local end of a tailnet connection against.
#[allow(dead_code)] // the web gate (phase 4)
pub fn tunnel_addresses<S: AsRef<str>>(prefixes: &[S]) -> Vec<IpAddr> {
    interface_addresses()
        .into_iter()
        .filter(|(name, _)| prefixes.iter().any(|p| name.starts_with(p.as_ref())))
        .map(|(_, ip)| ip)
        .collect()
}

/// This machine's Tailscale IPv4 addresses (100.64.0.0/10), for printing
/// the address a phone would use. Any interface: this is for display, not
/// for the gate.
pub fn tailnet_addresses() -> Vec<Ipv4Addr> {
    let mut found: Vec<Ipv4Addr> = interface_addresses()
        .into_iter()
        .filter_map(|(_, ip)| match ip {
            IpAddr::V4(v4) if is_cgnat(v4) => Some(v4),
            _ => None,
        })
        .collect();
    found.dedup();
    found
}

fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && o[1] & 0xc0 == 64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_is_listed() {
        let all = interface_addresses();
        assert!(all.iter().any(|(_, ip)| ip.is_loopback()));
        let lo = if cfg!(target_os = "macos") {
            "lo0"
        } else {
            "lo"
        };
        assert!(tunnel_addresses(&[lo]).iter().any(IpAddr::is_loopback));
        assert!(tunnel_addresses::<&str>(&[]).is_empty());
    }

    #[test]
    fn cgnat_range() {
        assert!(is_cgnat(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(is_cgnat(Ipv4Addr::new(100, 127, 255, 255)));
        assert!(!is_cgnat(Ipv4Addr::new(100, 128, 0, 1)));
        assert!(!is_cgnat(Ipv4Addr::new(100, 63, 0, 1)));
    }
}
