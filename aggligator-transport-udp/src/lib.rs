#![warn(missing_docs)]
#![doc = "Aggligator transport: UDP"]
//!
//! This crate provides a minimal UDP transport for [Aggligator](https://crates.io/crates/aggligator).
//!
//! ```no_run
//! use aggligator_transport_udp::simple::{udp_connect, udp_server};
//! ```

use aggligator::io::{StreamBox, TxRxBox};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use std::{
    any::Any,
    cmp::Ordering,
    collections::HashSet,
    fmt,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind, Result},
    net::SocketAddr,
    time::Duration,
};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, watch},
    time::sleep,
};
use tokio_util::{codec::BytesCodec, udp::UdpFramed};

use aggligator::{
    control::Direction,
    transport::{AcceptedStreamBox, AcceptingTransport, ConnectingTransport, LinkTag, LinkTagBox},
};

static NAME: &str = "udp";

/// IP protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum IpVersion {
    /// IPv4 only.
    IPv4,
    /// IPv6 only.
    IPv6,
    /// Both IPv4 and IPv6.
    #[default]
    Both,
}

impl IpVersion {
    fn is_only_ipv4(self) -> bool {
        matches!(self, Self::IPv4)
    }
    fn is_only_ipv6(self) -> bool {
        matches!(self, Self::IPv6)
    }
}

/// UDP link tag.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UdpLinkTag {
    /// Remote address.
    pub remote: SocketAddr,
    /// Link direction.
    pub direction: Direction,
}

impl fmt::Display for UdpLinkTag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let dir = match self.direction {
            Direction::Incoming => "<-",
            Direction::Outgoing => "->",
        };
        write!(f, "{dir} {}", self.remote)
    }
}

impl LinkTag for UdpLinkTag {
    fn transport_name(&self) -> &str {
        NAME
    }
    fn direction(&self) -> Direction {
        self.direction
    }
    fn user_data(&self) -> Vec<u8> {
        Vec::new()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn box_clone(&self) -> LinkTagBox {
        Box::new(self.clone())
    }
    fn dyn_cmp(&self, other: &dyn LinkTag) -> Ordering {
        let other = other.as_any().downcast_ref::<Self>().unwrap();
        Ord::cmp(self, other)
    }
    fn dyn_hash(&self, mut state: &mut dyn Hasher) {
        Hash::hash(self, &mut state)
    }
}

/// UDP transport for outgoing connections.
#[derive(Debug, Clone)]
pub struct UdpConnector {
    addrs: Vec<SocketAddr>,
    ip_version: IpVersion,
    resolve_interval: Duration,
}

impl UdpConnector {
    /// Create a new UDP transport for outgoing connections.
    pub async fn new(addrs: impl IntoIterator<Item = SocketAddr>) -> Result<Self> {
        let addrs: Vec<_> = addrs.into_iter().collect();
        if addrs.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput, "at least one address required"));
        }
        Ok(Self { addrs, ip_version: IpVersion::Both, resolve_interval: Duration::from_secs(30) })
    }
}

#[async_trait]
impl ConnectingTransport for UdpConnector {
    fn name(&self) -> &str {
        NAME
    }

    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()> {
        let mut tags: HashSet<LinkTagBox> = HashSet::new();
        for addr in &self.addrs {
            if (addr.is_ipv4() && self.ip_version.is_only_ipv6())
                || (addr.is_ipv6() && self.ip_version.is_only_ipv4())
            {
                continue;
            }
            tags.insert(Box::new(UdpLinkTag { remote: *addr, direction: Direction::Outgoing }) as LinkTagBox);
        }
        let _ = tx.send(tags);
        Ok(())
    }

    async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox> {
        let tag: &UdpLinkTag = tag.as_any().downcast_ref().unwrap();
        let local: SocketAddr = match tag.remote {
            SocketAddr::V4(_) => "0.0.0.0:0".parse().unwrap(),
            SocketAddr::V6(_) => "[::]:0".parse().unwrap(),
        };
        let socket = UdpSocket::bind(local).await?;
        socket.connect(tag.remote).await?;
        let framed = UdpFramed::new(socket, BytesCodec::new());
        let (sink, stream) = framed.split();
        let remote = tag.remote;
        let tx = sink.with(move |b: bytes::Bytes| async move { Ok((b, remote)) });
        let rx = stream.map(|r| r.map(|(b, _)| b.freeze()).map_err(|e| Error::other(e)));
        Ok(TxRxBox::new(tx, rx).into())
    }
}

/// UDP transport for incoming connections.
#[derive(Debug)]
pub struct UdpAcceptor {
    sockets: Vec<UdpSocket>,
}

impl UdpAcceptor {
    /// Create a new UDP transport listening on the specified addresses.
    pub async fn new(addrs: impl IntoIterator<Item = SocketAddr>) -> Result<Self> {
        let mut sockets = Vec::new();
        for addr in addrs {
            sockets.push(UdpSocket::bind(addr).await?);
        }
        if sockets.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput, "at least one socket required"));
        }
        Ok(Self { sockets })
    }
}

#[async_trait]
impl AcceptingTransport for UdpAcceptor {
    fn name(&self) -> &str {
        NAME
    }

    async fn listen(&self, tx: mpsc::Sender<AcceptedStreamBox>) -> Result<()> {
        let mut buf = vec![0u8; 2048];
        loop {
            for sock in &self.sockets {
                if let Ok((_len, remote)) = sock.recv_from(&mut buf).await {
                    let local = sock.local_addr()?;
                    let socket = UdpSocket::bind(local).await?;
                    socket.connect(remote).await?;
                    let framed = UdpFramed::new(socket, BytesCodec::new());
                    let (sink, stream) = framed.split();
                    let remote2 = remote;
                    let tx_rx = TxRxBox::new(
                        sink.with(move |b: bytes::Bytes| async move { Ok((b, remote2)) }),
                        stream.map(|r| r.map(|(b, _)| b.freeze()).map_err(|e| Error::other(e))),
                    );
                    let tag = UdpLinkTag { remote, direction: Direction::Incoming };
                    let _ = tx.send(AcceptedStreamBox::new(tx_rx.into(), tag)).await;
                }
            }
            sleep(Duration::from_millis(10)).await;
        }
    }
}

/// Simple functions for UDP connections.
pub mod simple {
    use super::*;
    use aggligator::{
        alc::Stream,
        exec,
        transport::{Acceptor, Connector},
    };
    use std::future::Future;

    /// Build a connection consisting of aggregated UDP links to the targets.
    pub async fn udp_connect(target: impl IntoIterator<Item = SocketAddr>) -> Result<Stream> {
        let mut connector = Connector::new();
        connector.add(UdpConnector::new(target).await?);
        let ch = connector.channel().unwrap().await?;
        Ok(ch.into_stream())
    }

    /// Run a UDP server accepting connections of aggregated UDP links.
    pub async fn udp_server<F>(
        addrs: impl IntoIterator<Item = SocketAddr>, work_fn: impl Fn(Stream) -> F + Send + 'static,
    ) -> Result<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let acceptor = Acceptor::new();
        acceptor.add(UdpAcceptor::new(addrs).await?);
        loop {
            let (ch, _control) = acceptor.accept().await?;
            exec::spawn(work_fn(ch.into_stream()));
        }
    }
}

/// Unordered UDP utilities.
pub mod unordered;
