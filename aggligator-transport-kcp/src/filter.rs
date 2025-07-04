/// KCP link filter method.
///
/// Controls which links are established between the local and remote endpoint.
#[derive(Default, Debug, Clone, Copy)]
pub enum KcpLinkFilter {
    /// No link filtering.
    /// Returns all combinations of local and remote addresses.
    #[default]
    None,
    /// Filter based on interface and IP address.
    /// One link for each pair of local interface and remote IP address.
    /// Excludes link-local addresses and ensures only one address per interface.
    InterfaceIp,
    /// Filter based on interface name.
    /// Similar to InterfaceIp but groups by interface name.
    InterfaceName,
}
