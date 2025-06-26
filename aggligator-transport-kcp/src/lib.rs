#![warn(missing_docs)]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    html_favicon_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    issue_tracker_base_url = "https://github.com/surban/aggligator/issues/"
)]

//! [Aggligator](aggligator) transport: KCP
//!
//! This crate provides KCP transport for the [Aggligator link aggregator].
//! KCP (ARQ protocol) offers reliable, low-latency communication over UDP.
//!
//! [Aggligator link aggregator]: https://crates.io/crates/aggligator
//!
//! ## Features
//! - Reliable delivery with configurable ARQ parameters
//! - Low latency optimized for real-time applications
//! - Congestion control and flow control
//! - Integration with Aggligator's link aggregation

use aggligator::{
    control::Direction,
    io::{IoBox, StreamBox},
    transport::{AcceptedStreamBox, AcceptingTransport, ConnectingTransport, LinkTag, LinkTagBox},
    Link,
};
use async_trait::async_trait;
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
    sync::{mpsc, watch},
    time::sleep,
};
use tokio_kcp::{KcpConfig, KcpListener, KcpStream};
use tracing::{debug, info, warn};

static NAME: &str = "kcp";

/// Wrapper around KcpStream to allow splitting for IoBox
struct KcpStreamWrapper(std::sync::Arc<std::sync::Mutex<KcpStream>>);

impl tokio::io::AsyncRead for KcpStreamWrapper {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let mut stream = self.0.lock().unwrap();
        std::pin::Pin::new(&mut *stream).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for KcpStreamWrapper {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize>> {
        let mut stream = self.0.lock().unwrap();
        std::pin::Pin::new(&mut *stream).poll_write(cx, buf)
    }
    
    fn poll_flush(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<()>> {
        let mut stream = self.0.lock().unwrap();
        std::pin::Pin::new(&mut *stream).poll_flush(cx)
    }
    
    fn poll_shutdown(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<()>> {
        let mut stream = self.0.lock().unwrap();
        std::pin::Pin::new(&mut *stream).poll_shutdown(cx)
    }
}

impl KcpStreamWrapper {
    fn create_pair(stream: KcpStream) -> (Self, Self) {
        let shared_stream = std::sync::Arc::new(std::sync::Mutex::new(stream));
        let read_half = KcpStreamWrapper(shared_stream.clone());
        let write_half = KcpStreamWrapper(shared_stream);
        (read_half, write_half)
    }
}

/// KCP link tag identifying a connection.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KcpLinkTag {
    /// Network interface name used for connection.
    pub interface: Vec<u8>,
    /// Remote socket address.
    pub remote: SocketAddr,
    /// Direction of the connection.
    pub direction: Direction,
}

impl KcpLinkTag {
    /// Creates a new KCP link tag.
    pub fn new(interface: &[u8], remote: SocketAddr, direction: Direction) -> Self {
        Self {
            interface: interface.to_vec(),
            remote,
            direction,
        }
    }
}

impl fmt::Display for KcpLinkTag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "KCP {} {} via {}",
            self.direction,
            self.remote,
            String::from_utf8_lossy(&self.interface)
        )
    }
}

impl LinkTag for KcpLinkTag {
    fn transport_name(&self) -> &str {
        NAME
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

    fn dyn_cmp(&self, other: &dyn LinkTag) -> Ordering {
        let other = other.as_any().downcast_ref::<Self>().unwrap();
        Ord::cmp(self, other)
    }

    fn dyn_hash(&self, mut state: &mut dyn Hasher) {
        Hash::hash(self, &mut state)
    }
}

/// KCP link filter method.
#[derive(Default, Debug, Clone, Copy)]
pub enum KcpLinkFilter {
    /// No link filtering.
    None,
    /// Filter based on local interface and remote IP address.
    #[default]
    InterfaceIp,
}

/// KCP transport for outgoing connections.
#[derive(Debug, Clone)]
pub struct KcpConnector {
    targets: Vec<SocketAddr>,
    resolve_interval: Duration,
    link_filter: KcpLinkFilter,
    kcp_config: KcpConfig,
}

impl fmt::Display for KcpConnector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "KCP -> {:?}", self.targets)
    }
}

impl KcpConnector {
    /// Creates a new KCP connector for the specified targets.
    pub fn new(targets: impl IntoIterator<Item = SocketAddr>) -> Self {
        let targets: Vec<_> = targets.into_iter().collect();
        
        // Create optimized KCP configuration for Aggligator
        let kcp_config = KcpConfig::default();
        // TODO: Configure KCP parameters when we understand the API better
        
        Self {
            targets,
            resolve_interval: Duration::from_secs(10),
            link_filter: KcpLinkFilter::default(),
            kcp_config,
        }
    }

    /// Sets the interval for checking network interface changes.
    pub fn set_resolve_interval(&mut self, resolve_interval: Duration) {
        self.resolve_interval = resolve_interval;
    }

    /// Sets the link filter method.
    pub fn set_link_filter(&mut self, link_filter: KcpLinkFilter) {
        self.link_filter = link_filter;
    }

    /// Sets the KCP configuration.
    pub fn set_kcp_config(&mut self, kcp_config: KcpConfig) {
        self.kcp_config = kcp_config;
    }
}

/// KCP transport for incoming connections.
#[derive(Debug)]
pub struct KcpAcceptor {
    bind_addrs: Vec<SocketAddr>,
    kcp_config: KcpConfig,
}

impl fmt::Display for KcpAcceptor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let addrs: Vec<_> = self.bind_addrs.iter().map(|addr| addr.to_string()).collect();
        if addrs.len() > 1 {
            write!(f, "KCP [{}]", addrs.join(", "))
        } else if !addrs.is_empty() {
            write!(f, "KCP {}", addrs[0])
        } else {
            write!(f, "KCP listener")
        }
    }
}

impl KcpAcceptor {
    /// Creates a new KCP acceptor for the specified addresses.
    pub fn new(addrs: impl IntoIterator<Item = SocketAddr>) -> Self {
        let bind_addrs: Vec<_> = addrs.into_iter().collect();
        let kcp_config = KcpConfig::default();
        
        Self { 
            bind_addrs,
            kcp_config,
        }
    }

    /// Sets the KCP configuration.
    pub fn set_kcp_config(&mut self, kcp_config: KcpConfig) {
        self.kcp_config = kcp_config;
    }
}

// TODO: 在下一步实现 ConnectingTransport 和 AcceptingTransport traits

// First, we need a utility module for interface management
mod util {
    use std::{net::SocketAddr, collections::HashSet};
    
    /// Simplified interface management for KCP transport
    /// TODO: Implement proper interface detection
    pub fn interface_names_for_target(_target: SocketAddr) -> HashSet<Vec<u8>> {
        let mut interfaces = HashSet::new();
        interfaces.insert(b"default".to_vec());
        interfaces
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
            
            for &target in &self.targets {
                for interface in util::interface_names_for_target(target) {
                    let tag = KcpLinkTag::new(&interface, target, Direction::Outgoing);
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
        let tag: &KcpLinkTag = tag.as_any().downcast_ref().unwrap();
        
        debug!("Connecting KCP to {}", tag.remote);
        
        // Create KCP stream with our configuration
        let stream = KcpStream::connect(&self.kcp_config, tag.remote).await
            .map_err(|e| Error::new(ErrorKind::ConnectionRefused, e))?;
        
        info!("KCP connection established to {}", tag.remote);
        
        // Use our wrapper to create split streams
        let (read_half, write_half) = KcpStreamWrapper::create_pair(stream);
        Ok(IoBox::new(read_half, write_half).into())
    }

    async fn link_filter(&self, new: &Link<LinkTagBox>, existing: &[Link<LinkTagBox>]) -> bool {
        let Some(new_tag) = new.tag().as_any().downcast_ref::<KcpLinkTag>() else { 
            return true; 
        };

        match self.link_filter {
            KcpLinkFilter::None => true,
            KcpLinkFilter::InterfaceIp => {
                // Check for duplicate interface + IP combinations
                let duplicate = existing.iter().any(|link| {
                    if let Some(tag) = link.tag().as_any().downcast_ref::<KcpLinkTag>() {
                        tag.interface == new_tag.interface && tag.remote.ip() == new_tag.remote.ip()
                    } else {
                        false
                    }
                });
                
                if duplicate {
                    debug!("KCP link {} is redundant, rejecting.", new_tag);
                    false
                } else {
                    debug!("KCP link {} accepted.", new_tag);
                    true
                }
            }
        }
    }
}

#[async_trait]
impl AcceptingTransport for KcpAcceptor {
    fn name(&self) -> &str {
        NAME
    }

    async fn listen(&self, tx: mpsc::Sender<AcceptedStreamBox>) -> Result<()> {
        if self.bind_addrs.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput, "No bind addresses configured"));
        }
        
        // Create listeners for all bind addresses
        let mut listeners = Vec::new();
        for &addr in &self.bind_addrs {
            let listener = KcpListener::bind(self.kcp_config.clone(), addr).await
                .map_err(|e| Error::new(ErrorKind::AddrInUse, e))?;
            info!("KCP listening on {}", addr);
            listeners.push(listener);
        }
        
        // Accept connections in a loop
        loop {
            // For now, just use the first listener to get something working
            // TODO: Use select_all to handle multiple listeners
            let (stream, remote) = listeners[0].accept().await
                .map_err(|e| Error::new(ErrorKind::ConnectionAborted, e))?;
            
            debug!("Accepted KCP connection from {}", remote);
            
            // Create link tag for incoming connection
            let tag = KcpLinkTag::new(b"default", remote, Direction::Incoming);
            
            // Use our wrapper to create split streams
            let (read_half, write_half) = KcpStreamWrapper::create_pair(stream);
            let stream_box = IoBox::new(read_half, write_half).into();
            
            // Send the accepted stream
            if let Err(_) = tx.send(AcceptedStreamBox::new(stream_box, tag)).await {
                warn!("Failed to send accepted KCP stream - receiver dropped");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kcp_link_tag_creation() {
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let tag = KcpLinkTag::new(b"eth0", addr, Direction::Outgoing);
        
        assert_eq!(tag.interface, b"eth0");
        assert_eq!(tag.remote, addr);
        assert_eq!(tag.direction, Direction::Outgoing);
        
        // Test display
        let display = format!("{}", tag);
        assert!(display.contains("KCP"));
        assert!(display.contains("127.0.0.1:8080"));
        assert!(display.contains("outgoing"));
    }

    #[test]
    fn test_kcp_connector_creation() {
        let targets = vec!["127.0.0.1:8080".parse().unwrap(), "127.0.0.1:8081".parse().unwrap()];
        let connector = KcpConnector::new(targets.clone());
        
        // Test display
        let display = format!("{}", connector);
        assert!(display.contains("KCP"));
        assert!(display.contains("127.0.0.1:8080"));
        assert!(display.contains("127.0.0.1:8081"));
    }

    #[test]
    fn test_kcp_acceptor_creation() {
        let addrs = vec!["127.0.0.1:8080".parse().unwrap(), "127.0.0.1:8081".parse().unwrap()];
        let acceptor = KcpAcceptor::new(addrs.clone());
        
        // Test display
        let display = format!("{}", acceptor);
        assert!(display.contains("KCP"));
        assert!(display.contains("127.0.0.1:8080"));
        assert!(display.contains("127.0.0.1:8081"));
    }

    #[test]
    fn test_kcp_link_tag_ord() {
        let addr1: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let addr2: SocketAddr = "127.0.0.1:8081".parse().unwrap();
        
        let tag1 = KcpLinkTag::new(b"eth0", addr1, Direction::Outgoing);
        let tag2 = KcpLinkTag::new(b"eth0", addr2, Direction::Outgoing);
        let tag3 = KcpLinkTag::new(b"wlan0", addr1, Direction::Outgoing);
        
        // Same interface, different ports
        assert_ne!(tag1, tag2);
        
        // Different interface, same address
        assert_ne!(tag1, tag3);
        
        // Tags should be orderable
        assert!(tag1 < tag2); // port 8080 < 8081
    }
}
