//! UDP connector implementation with multi-interface support
//!
//! This module provides a UDP connector that automatically discovers local network
//! interfaces and creates multiple UDP links for true multi-link aggregation.

use aggligator::transport::{ConnectingTransport, LinkTag, LinkTagBox};
use aggligator::io::{StreamBox, TxRxBox};
use aggligator::control::{Direction, Link};
use async_trait::async_trait;
use std::{
    any::Any,
    collections::HashSet,
    fmt,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind, Result},
    net::SocketAddr,
    time::Duration,
};
use tokio::{net::UdpSocket, time::sleep};
use tokio::sync::watch;

pub use aggligator_transport_tcp::util::{local_interfaces, interface_names_for_target, NetworkInterface};

/// UDP link tag representing a specific interface and target combination
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UdpLinkTag {
    /// Network interface name
    pub interface: Vec<u8>,
    /// Remote target address
    pub remote: SocketAddr,
    /// Connection direction
    pub direction: Direction,
}

impl UdpLinkTag {
    /// Create a new UDP link tag
    pub fn new(interface: &[u8], remote: SocketAddr, direction: Direction) -> Self {
        Self {
            interface: interface.to_vec(),
            remote,
            direction,
        }
    }
}

impl fmt::Display for UdpLinkTag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "UDP {} {} via {}",
            match self.direction {
                Direction::Incoming => "from",
                Direction::Outgoing => "to",
            },
            self.remote,
            String::from_utf8_lossy(&self.interface)
        )
    }
}

impl LinkTag for UdpLinkTag {
    fn transport_name(&self) -> &str {
        "UDP"
    }

    fn direction(&self) -> Direction {
        self.direction
    }

    fn user_data(&self) -> Vec<u8> {
        self.interface.clone()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn box_clone(&self) -> LinkTagBox {
        Box::new(self.clone())
    }

    fn dyn_cmp(&self, other: &dyn LinkTag) -> std::cmp::Ordering {
        let other = other.as_any().downcast_ref::<Self>().unwrap();
        Ord::cmp(self, other)
    }

    fn dyn_hash(&self, mut state: &mut dyn Hasher) {
        Hash::hash(self, &mut state)
    }
}

/// UDP transport for outgoing connections with multi-interface support
#[derive(Debug, Clone)]
pub struct UdpConnector {
    targets: Vec<SocketAddr>,
    resolve_interval: Duration,
}

impl fmt::Display for UdpConnector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "UDP to {:?}", self.targets)
    }
}

impl UdpConnector {
    /// Create a new UDP connector for the specified targets
    pub fn new(targets: impl IntoIterator<Item = SocketAddr>) -> Self {
        Self {
            targets: targets.into_iter().collect(),
            resolve_interval: Duration::from_secs(10),
        }
    }

    /// Set the interval for checking network interface changes
    pub fn set_resolve_interval(&mut self, resolve_interval: Duration) {
        self.resolve_interval = resolve_interval;
    }

    /// Bind UDP socket to a specific interface for the given target
    async fn bind_to_interface(interface: &NetworkInterface, target: SocketAddr) -> Result<UdpSocket> {
        // Find a suitable IP address on this interface for the target
        let local_ip = interface
            .addr
            .iter()
            .find_map(|addr| {
                let ip = addr.ip();
                if !ip.is_unspecified()
                    && ip.is_loopback() == target.ip().is_loopback()
                    && ip.is_ipv4() == target.is_ipv4()
                    && ip.is_ipv6() == target.is_ipv6()
                {
                    Some(ip)
                } else {
                    None
                }
            })
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "no suitable IP address on interface"))?;

        let local_addr = SocketAddr::new(local_ip, 0);
        let socket = UdpSocket::bind(local_addr).await?;

        // Connect to target to establish the UDP "connection"
        socket.connect(target).await?;

        tracing::debug!(
            "UDP bound to {} on interface {} for target {}",
            socket.local_addr()?,
            String::from_utf8_lossy(interface.name.as_bytes()),
            target
        );

        Ok(socket)
    }
}

#[async_trait]
impl ConnectingTransport for UdpConnector {
    fn name(&self) -> &str {
        "UDP"
    }

    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()> {
        loop {
            let interfaces = local_interfaces()?;

            let mut tags: HashSet<LinkTagBox> = HashSet::new();
            for &target in &self.targets {
                for iface_name in interface_names_for_target(&interfaces, target) {
                    let tag = UdpLinkTag::new(&iface_name, target, Direction::Outgoing);
                    tags.insert(Box::new(tag));
                }
            }

            tx.send_if_modified(|v| {
                if *v != tags {
                    *v = tags;
                    true
                } else {
                    false
                }
            });

            sleep(self.resolve_interval).await;
        }
    }

    async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox> {
        let tag: &UdpLinkTag = tag.as_any().downcast_ref().unwrap();

        // Find the interface
        let interfaces = local_interfaces()?;
        let interface = interfaces
            .iter()
            .find(|iface| iface.name.as_bytes() == tag.interface)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "network interface not found"))?;

        // Create and bind UDP socket to this interface
        let socket = Self::bind_to_interface(interface, tag.remote).await?;
        
        // Create UDP Tx/Rx streams
        let socket = std::sync::Arc::new(socket);
        let tx = UdpTx::new(socket.clone());
        let rx = UdpRx::new(socket);
        Ok(TxRxBox::new(tx, rx).into())
    }

    async fn link_filter(&self, new: &Link<LinkTagBox>, existing: &[Link<LinkTagBox>]) -> bool {
        let Some(new_tag) = new.tag().as_any().downcast_ref::<UdpLinkTag>() else { return true };

        // Check if we already have a link with the same interface and target
        let duplicate = existing.iter().any(|link| {
            if let Some(tag) = link.tag().as_any().downcast_ref::<UdpLinkTag>() {
                tag.interface == new_tag.interface && tag.remote == new_tag.remote
            } else {
                false
            }
        });

        if duplicate {
            tracing::debug!("UDP link {} is redundant, rejecting.", new_tag);
            false
        } else {
            tracing::debug!("UDP link {} accepted.", new_tag);
            true
        }
    }
}

use bytes::Bytes;
use futures::{Sink, Stream};
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::io::{self, ReadBuf};

/// UDP transmitter implementing Sink<Bytes>
struct UdpTx {
    socket: Arc<UdpSocket>,
}

impl UdpTx {
    fn new(socket: Arc<UdpSocket>) -> Self {
        Self { socket }
    }
}

impl Sink<Bytes> for UdpTx {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> std::result::Result<(), Self::Error> {
        // For UDP, we need to use try_send since we can't block
        // In a real implementation, you might want to buffer this
        match self.socket.try_send(&item) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // UDP socket not ready, should be handled by buffering
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        // UDP doesn't need flushing
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

/// UDP receiver implementing Stream<Item = io::Result<Bytes>>
struct UdpRx {
    socket: Arc<UdpSocket>,
    buffer: Vec<u8>,
}

impl UdpRx {
    fn new(socket: Arc<UdpSocket>) -> Self {
        Self {
            socket,
            buffer: vec![0u8; 65536], // Maximum UDP packet size
        }
    }
}

impl Stream for UdpRx {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let mut read_buf = ReadBuf::new(&mut this.buffer);
        match this.socket.poll_recv(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let filled = read_buf.filled();
                let data = Bytes::copy_from_slice(filled);
                Poll::Ready(Some(Ok(data)))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}
