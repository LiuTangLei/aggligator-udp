//! Utils for working with KCP connections.

use network_interface::NetworkInterfaceConfig;
use std::{
    collections::{HashMap, HashSet},
    io::{Error, Result},
    net::{IpAddr, SocketAddr},
};
use tokio::net::UdpSocket;

pub use network_interface::NetworkInterface;

use crate::KcpLinkFilter;

/// IP version preference.
#[derive(Default, Debug, Clone, Copy)]
pub enum IpVersion {
    /// IP version 4.
    IPv4,
    /// IP version 6.
    IPv6,
    /// Both IP versions.
    #[default]
    Both,
}

impl IpVersion {
    /// Create from "only" arguments.
    pub fn from_only(only_ipv4: bool, only_ipv6: bool) -> Result<Self> {
        match (only_ipv4, only_ipv6) {
            (false, false) => Ok(Self::Both),
            (true, false) => Ok(Self::IPv4),
            (false, true) => Ok(Self::IPv6),
            (true, true) => {
                Err(Error::new(std::io::ErrorKind::InvalidInput, "IPv4 and IPv6 options are mutually exclusive"))
            }
        }
    }

    /// Check if this only supports IPv4.
    pub fn is_only_ipv4(&self) -> bool {
        matches!(self, Self::IPv4)
    }

    /// Check if this only supports IPv6.
    pub fn is_only_ipv6(&self) -> bool {
        matches!(self, Self::IPv6)
    }
}

/// Gets the list of local network interfaces from the operating system.
///
/// Filters out interfaces that are most likely useless.
pub fn local_interfaces() -> Result<Vec<NetworkInterface>> {
    Ok(NetworkInterface::show()
        .map_err(|err| Error::other(err.to_string()))?
        .into_iter()
        .filter(|iface| !iface.name.starts_with("ifb"))
        .collect())
}

/// Translate IPv4 address mapped to an IPv6 alias into proper IPv4 address.
pub fn use_proper_ipv4(sa: &mut SocketAddr) {
    if let IpAddr::V6(addr) = sa.ip() {
        if let Some(addr) = addr.to_ipv4_mapped() {
            sa.set_ip(addr.into());
        }
    }
}

/// Gets the local interface for the specified IP address.
pub fn local_interface_for_ip(ip: IpAddr) -> Result<Option<Vec<u8>>> {
    let interfaces = local_interfaces()?;
    let iface = interfaces.into_iter().find_map(|interface| {
        interface.addr.iter().any(|addr| addr.ip() == ip).then_some(interface.name.into_bytes())
    });
    Ok(iface)
}

/// Returns all local addresses for the specified target.
///
/// This function returns a set of local addresses that can be used to connect 
/// to the specified target address, taking into account IP version compatibility.
pub fn local_addresses_for_target(target: SocketAddr) -> Result<HashSet<SocketAddr>> {
    let interfaces = local_interfaces()?;
    let mut local_addrs = HashSet::new();

    for interface in interfaces {
        for addr in interface.addr {
            // Check IP version compatibility
            match (addr.ip(), target.ip()) {
                (IpAddr::V4(_), IpAddr::V4(_)) => (),
                (IpAddr::V6(_), IpAddr::V6(_)) => (),
                _ => continue,
            }

            // Check loopback compatibility
            if addr.ip().is_loopback() != target.ip().is_loopback() {
                continue;
            }

            // Skip unspecified addresses
            if addr.ip().is_unspecified() {
                continue;
            }

            // Add the local address with port 0 (system assigned)
            local_addrs.insert(SocketAddr::new(addr.ip(), 0));
        }
    }

    // If no specific local addresses found, add unspecified addresses for fallback
    if local_addrs.is_empty() {
        match target.ip() {
            IpAddr::V4(_) => {
                local_addrs.insert("0.0.0.0:0".parse().unwrap());
            }
            IpAddr::V6(_) => {
                local_addrs.insert("[::]:0".parse().unwrap());
            }
        }
    }

    Ok(local_addrs)
}

/// Binds a UDP socket to the specified local address.
///
/// If the local address is unspecified (0.0.0.0 or ::), the socket is bound
/// with automatic address assignment.
pub async fn bind_udp_socket(local: SocketAddr) -> Result<UdpSocket> {
    UdpSocket::bind(local).await
}

/// Plans the links to be established based on filter strategy.
///
/// Returns a set of (local, remote) address pairs that should be used for establishing links.
/// Limits to maximum 4 links per (local, remote) pair to avoid resource exhaustion.
pub fn plan_links(
    filter: KcpLinkFilter,
    locals: &[SocketAddr],
    remotes: &[SocketAddr],
) -> Result<HashSet<(SocketAddr, SocketAddr)>> {
    let mut links = HashSet::new();

    match filter {
        KcpLinkFilter::None => {
            // Return all combinations of local and remote addresses
            for &local in locals {
                for &remote in remotes {
                    links.insert((local, remote));
                }
            }
        }
        KcpLinkFilter::InterfaceIp | KcpLinkFilter::InterfaceName => {
            // Get network interfaces and filter addresses
            let interfaces = local_interfaces()?;
            let filtered_locals = filter_local_addresses(&interfaces, filter)?;

            for local in filtered_locals {
                for &remote in remotes {
                    // Ensure IP version compatibility
                    if is_compatible_ip_version(local.ip(), remote.ip()) {
                        links.insert((local, remote));
                    }
                }
            }
        }
    }

    // Limit to maximum 4 links per (local, remote) pair
    // Group by (local_ip, remote) and take at most 4
    let mut grouped: HashMap<(IpAddr, SocketAddr), Vec<(SocketAddr, SocketAddr)>> = HashMap::new();
    for (local, remote) in links {
        grouped
            .entry((local.ip(), remote))
            .or_default()
            .push((local, remote));
    }

    let mut result = HashSet::new();
    for links_group in grouped.values() {
        for &link in links_group.iter().take(4) {
            result.insert(link);
        }
    }

    Ok(result)
}

/// Filters local addresses based on the filter strategy.
///
/// Excludes link-local addresses (fe80::/64) and ensures only one address per interface.
fn filter_local_addresses(
    interfaces: &[NetworkInterface],
    filter: KcpLinkFilter,
) -> Result<HashSet<SocketAddr>> {
    let mut local_addrs = HashSet::new();
    let mut interface_groups: HashMap<String, Vec<SocketAddr>> = HashMap::new();

    // Group addresses by interface
    for interface in interfaces {
        let mut iface_addrs = Vec::new();
        
        for addr in &interface.addr {
            let ip = addr.ip();
            
            // Skip link-local addresses (fe80::/64)
            if is_link_local(ip) {
                continue;
            }
            
            // Skip unspecified addresses
            if ip.is_unspecified() {
                continue;
            }
            
            iface_addrs.push(SocketAddr::new(ip, 0));
        }
        
        if !iface_addrs.is_empty() {
            interface_groups.insert(interface.name.clone(), iface_addrs);
        }
    }

    // Select one address per interface based on filter strategy
    for (_iface_name, addrs) in interface_groups {
        let selected = match filter {
            KcpLinkFilter::InterfaceIp | KcpLinkFilter::InterfaceName => {
                // Prefer IPv6 over IPv4, then prefer non-loopback over loopback
                addrs
                    .into_iter()
                    .max_by_key(|addr| {
                        let ip = addr.ip();
                        let ipv6_preference = if ip.is_ipv6() { 2 } else { 1 };
                        let loopback_preference = if ip.is_loopback() { 0 } else { 1 };
                        (ipv6_preference, loopback_preference)
                    })
            }
            KcpLinkFilter::None => {
                // This branch shouldn't be reached, but handle it gracefully
                addrs.into_iter().next()
            }
        };
        
        if let Some(addr) = selected {
            local_addrs.insert(addr);
        }
    }

    // If no filtered addresses found, add fallback unspecified addresses
    if local_addrs.is_empty() {
        local_addrs.insert("0.0.0.0:0".parse().unwrap());
        local_addrs.insert("[::]:0".parse().unwrap());
    }

    Ok(local_addrs)
}

/// Checks if an IP address is link-local.
///
/// For IPv6, this checks if the address is in fe80::/64 range.
/// For IPv4, this checks if the address is in 169.254.0.0/16 range.
fn is_link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V6(v6) => {
            // Check if address is in fe80::/64 range
            v6.segments()[0] == 0xfe80
        }
        IpAddr::V4(v4) => {
            // Check if address is in 169.254.0.0/16 range (IPv4 link-local)
            let octets = v4.octets();
            octets[0] == 169 && octets[1] == 254
        }
    }
}

/// Checks if two IP addresses are compatible for establishing a connection.
fn is_compatible_ip_version(local: IpAddr, remote: IpAddr) -> bool {
    match (local, remote) {
        (IpAddr::V4(_), IpAddr::V4(_)) => true,
        (IpAddr::V6(_), IpAddr::V6(_)) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_is_link_local() {
        // IPv6 link-local addresses
        assert!(is_link_local(IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1))));
        assert!(is_link_local(IpAddr::V6(Ipv6Addr::new(0xfe80, 0x1234, 0, 0, 0, 0, 0, 1))));
        
        // IPv6 non-link-local addresses
        assert!(!is_link_local(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1))));
        assert!(!is_link_local(IpAddr::V6(Ipv6Addr::LOCALHOST)));

        // IPv4 link-local addresses
        assert!(is_link_local(IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))));
        assert!(is_link_local(IpAddr::V4(Ipv4Addr::new(169, 254, 255, 255))));
        
        // IPv4 non-link-local addresses
        assert!(!is_link_local(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(!is_link_local(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    #[test]
    fn test_is_compatible_ip_version() {
        let ipv4_1 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));
        let ipv4_2 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let ipv6_1 = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        let ipv6_2 = IpAddr::V6(Ipv6Addr::LOCALHOST);

        assert!(is_compatible_ip_version(ipv4_1, ipv4_2));
        assert!(is_compatible_ip_version(ipv6_1, ipv6_2));
        assert!(!is_compatible_ip_version(ipv4_1, ipv6_1));
        assert!(!is_compatible_ip_version(ipv6_1, ipv4_1));
    }

    #[test]
    fn test_plan_links_none_filter() {
        let locals = vec![
            "192.168.1.100:0".parse().unwrap(),
            "10.0.0.100:0".parse().unwrap(),
        ];
        let remotes = vec![
            "192.168.1.1:8080".parse().unwrap(),
            "10.0.0.1:8080".parse().unwrap(),
        ];

        let links = plan_links(KcpLinkFilter::None, &locals, &remotes).unwrap();
        
        // Should return all combinations: 2 locals × 2 remotes = 4 links
        assert_eq!(links.len(), 4);
        
        // Verify all combinations are present
        for &local in &locals {
            for &remote in &remotes {
                assert!(links.contains(&(local, remote)));
            }
        }
    }

    #[test]
    fn test_plan_links_interface_filter() {
        let locals = vec![
            "192.168.1.100:0".parse().unwrap(),
            "10.0.0.100:0".parse().unwrap(),
            "[fe80::1]:0".parse().unwrap(), // link-local, should be filtered out
        ];
        let remotes = vec![
            "192.168.1.1:8080".parse().unwrap(),
            "10.0.0.1:8080".parse().unwrap(),
        ];

        let links = plan_links(KcpLinkFilter::InterfaceIp, &locals, &remotes).unwrap();
        
        // Should filter out link-local addresses and maintain IP version compatibility
        for (local, _remote) in &links {
            assert!(!is_link_local(local.ip()));
        }
    }

    #[test] 
    fn test_plan_links_max_4_per_pair() {
        let locals: Vec<SocketAddr> = (0..10)
            .map(|i| format!("192.168.1.{}:0", 100 + i).parse().unwrap())
            .collect();
        let remotes = vec!["192.168.1.1:8080".parse().unwrap()];

        let links = plan_links(KcpLinkFilter::None, &locals, &remotes).unwrap();
        
        // Should be limited to 4 links maximum per (local_ip, remote) pair
        // But since all locals have different IPs, we should get all 10
        assert_eq!(links.len(), 10);
        
        // Test with same IP but different ports (which shouldn't happen in practice)
        let locals_same_ip: Vec<SocketAddr> = (0..10)
            .map(|i| format!("192.168.1.100:{}", 1000 + i).parse().unwrap())
            .collect();
            
        let links_same_ip = plan_links(KcpLinkFilter::None, &locals_same_ip, &remotes).unwrap();
        
        // Should be limited to 4 since they all have the same IP
        assert!(links_same_ip.len() <= 4);
    }
}
