use std::{
    collections::HashSet,
    io::Result,
    net::SocketAddr,
    time::Duration,
};

use async_trait::async_trait;
use tokio::sync::watch;
use tokio_kcp::{KcpConfig, KcpStream};

use aggligator::{
    control::Direction,
    io::{IoBox, StreamBox},
    transport::{ConnectingTransport, LinkTag, LinkTagBox},
};

use crate::{link_tag::KcpLinkTag, util, KcpLinkFilter, NAME};

/// KCP transport for outgoing connections.
///
/// This transport is stream-based and supports multiple target addresses.
#[derive(Debug, Clone)]
pub struct KcpConnector {
    targets: Vec<SocketAddr>,
    cfg: KcpConfig,
    filter: KcpLinkFilter,
    resolve_interval: Duration,
}

impl KcpConnector {
    /// Create a new KCP transport for outgoing connections.
    pub fn new(cfg: KcpConfig, targets: Vec<SocketAddr>) -> Self {
        Self { 
            cfg, 
            targets,
            filter: KcpLinkFilter::default(),
            resolve_interval: Duration::from_secs(10),
        }
    }

    /// Gets the KCP configuration.
    pub fn config(&self) -> &KcpConfig {
        &self.cfg
    }

    /// Gets the target addresses.
    pub fn targets(&self) -> &[SocketAddr] {
        &self.targets
    }

    /// Sets the link filter method.
    pub fn set_filter(&mut self, filter: KcpLinkFilter) {
        self.filter = filter;
    }

    /// Sets the interval for re-checking network interfaces.
    pub fn set_resolve_interval(&mut self, resolve_interval: Duration) {
        self.resolve_interval = resolve_interval;
    }
}

#[async_trait]
impl ConnectingTransport for KcpConnector {
    fn name(&self) -> &str {
        NAME
    }

    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()> {
        loop {
            let mut tags: HashSet<LinkTagBox> = HashSet::new();

            // Prepare local addresses based on filter
            let locals = match self.filter {
                KcpLinkFilter::None => {
                    // For None filter, use unspecified addresses for each target IP version
                    let mut locals = Vec::new();
                    for &target in &self.targets {
                        let local = match target.ip() {
                            std::net::IpAddr::V4(_) => "0.0.0.0:0".parse().unwrap(),
                            std::net::IpAddr::V6(_) => "[::]:0".parse().unwrap(),
                        };
                        locals.push(local);
                    }
                    locals
                }
                KcpLinkFilter::InterfaceIp | KcpLinkFilter::InterfaceName => {
                    // Get all available local addresses
                    let mut locals = Vec::new();
                    let interfaces = util::local_interfaces()?;
                    for interface in interfaces {
                        for addr in interface.addr {
                            if !addr.ip().is_unspecified() {
                                locals.push(SocketAddr::new(addr.ip(), 0));
                            }
                        }
                    }
                    if locals.is_empty() {
                        locals.push("0.0.0.0:0".parse().unwrap());
                        locals.push("[::]:0".parse().unwrap());
                    }
                    locals
                }
            };

            // Use plan_links to get the optimal set of links
            let planned_links = util::plan_links(self.filter, &locals, &self.targets)?;

            // Convert planned links to tags
            for (local, remote) in planned_links {
                let tag = KcpLinkTag::new(local, remote, Direction::Outgoing);
                tags.insert(Box::new(tag) as LinkTagBox);
            }

            tx.send_if_modified(|v| {
                if *v != tags {
                    *v = tags;
                    true
                } else {
                    false
                }
            });

            tokio::time::sleep(self.resolve_interval).await;
        }
    }

    async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox> {
        let tag: &KcpLinkTag = tag.as_any().downcast_ref().unwrap();

        let stream = if tag.local.ip().is_unspecified() {
            // Direct connect without binding to specific local address
            KcpStream::connect(&self.cfg, tag.remote).await?
        } else {
            // Bind to specific local address first
            let udp = util::bind_udp_socket(tag.local).await?;
            KcpStream::connect_with_socket(&self.cfg, udp, tag.remote).await?
        };

        let (r, w) = tokio::io::split(stream);
        Ok(IoBox::new(r, w).into())
    }
}
