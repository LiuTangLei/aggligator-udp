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

    /// Optimize socket for low latency instead of high throughput
    fn optimize_socket_buffers(_socket: &UdpSocket) -> Result<()> {
        // Keep it simple - no buffer modifications needed
        // Focus on low latency rather than high throughput
        tracing::debug!("Socket optimization skipped for low latency");
        Ok(())
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
        
        // Optimize socket buffer sizes for high throughput
        Self::optimize_socket_buffers(&socket)?;
        
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
    collections::VecDeque,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};
use tokio::io::{self, ReadBuf};

/// UDP transmitter implementing Sink<Bytes> with send buffering
struct UdpTx {
    socket: Arc<UdpSocket>,
    /// Send buffer to handle WouldBlock scenarios
    send_buffer: VecDeque<Bytes>,
    /// Maximum buffer size to prevent memory exhaustion
    max_buffer_size: usize,
    /// Waker for when socket becomes writable
    waker: Option<Waker>,
}

impl UdpTx {
    fn new(socket: Arc<UdpSocket>) -> Self {
        Self { 
            socket,
            send_buffer: VecDeque::new(),
            max_buffer_size: 1024, // Maximum 1024 packets in buffer
            waker: None,
        }
    }

    /// Try to flush the send buffer
    fn try_flush_buffer(&mut self) -> std::result::Result<bool, io::Error> {
        while let Some(data) = self.send_buffer.front() {
            match self.socket.try_send(data) {
                Ok(_) => {
                    self.send_buffer.pop_front();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(false); // Socket not ready, stop flushing
                }
                Err(e) => return Err(e),
            }
        }
        Ok(true) // All data flushed
    }
}

impl Sink<Bytes> for UdpTx {
    type Error = io::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        // Try to flush existing buffer first
        match self.try_flush_buffer() {
            Ok(true) => {
                // Buffer completely flushed
                if self.send_buffer.len() < self.max_buffer_size {
                    Poll::Ready(Ok(()))
                } else {
                    // Buffer full, register waker and return pending
                    self.waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
            Ok(false) => {
                // Socket not ready, register waker and return pending
                self.waker = Some(cx.waker().clone());
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn start_send(mut self: Pin<&mut Self>, item: Bytes) -> std::result::Result<(), Self::Error> {
        // Try to send directly first if buffer is empty
        if self.send_buffer.is_empty() {
            match self.socket.try_send(&item) {
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Socket not ready, fall through to buffering
                }
                Err(e) => return Err(e),
            }
        }

        // Buffer the data if we can't send directly
        if self.send_buffer.len() < self.max_buffer_size {
            self.send_buffer.push_back(item);
            Ok(())
        } else {
            // Buffer is full, drop the packet (or return error)
            tracing::warn!("UDP send buffer full, dropping packet");
            Ok(()) // Consider returning an error here if you want backpressure
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        match self.try_flush_buffer() {
            Ok(true) => Poll::Ready(Ok(())),
            Ok(false) => {
                self.waker = Some(cx.waker().clone());
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        // Flush all remaining data before closing
        match self.try_flush_buffer() {
            Ok(true) => Poll::Ready(Ok(())),
            Ok(false) => {
                self.waker = Some(cx.waker().clone());
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
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
