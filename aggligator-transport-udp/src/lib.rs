//! UDP transport implementation for Aggligator unordered aggregation.
//!
//! This module provides a concrete implementation of the `UnorderedLinkTransport`
//! trait using UDP sockets, enabling efficient unordered packet aggregation
//! over UDP connections.

use std::{fmt, io, net::SocketAddr, sync::Arc};

use tokio::net::UdpSocket;
use tracing::{debug, error, info};

use aggligator::unordered_task::UnorderedLinkTransport;

pub mod connector;

/// UDP transport implementation for unordered aggregation
#[derive(Debug)]
pub struct UdpTransport {
    /// UDP socket for communication
    socket: Arc<UdpSocket>,
    /// Remote address for this transport
    remote_addr: SocketAddr,
    /// Local address bound to the socket
    local_addr: SocketAddr,
}

impl UdpTransport {
    /// Create a new UDP transport bound to the specified local address
    pub async fn new(local_addr: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(local_addr).await?;
        let actual_local_addr = socket.local_addr()?;

        info!("UDP transport bound to {}", actual_local_addr);

        Ok(Self {
            socket: Arc::new(socket),
            remote_addr: "0.0.0.0:0".parse().unwrap(), // Will be set when connecting
            local_addr: actual_local_addr,
        })
    }

    /// Connect this transport to a remote address
    pub async fn connect_to(&mut self, remote_addr: SocketAddr) -> io::Result<()> {
        self.socket.connect(remote_addr).await?;
        self.remote_addr = remote_addr;

        info!("UDP transport connected to {}", remote_addr);
        Ok(())
    }

    /// Create a new UDP transport and connect to the specified remote address
    pub async fn connect(local_addr: SocketAddr, remote_addr: SocketAddr) -> io::Result<Self> {
        let mut transport = Self::new(local_addr).await?;
        transport.connect_to(remote_addr).await?;
        Ok(transport)
    }

    /// Create a new UDP transport that connects to a remote address without explicit local binding
    /// This lets the system choose the best local interface and port
    pub async fn new_unbound(remote_addr: SocketAddr) -> io::Result<Self> {
        // Create an unbound socket first
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        // Connect to establish the local address automatically
        socket.connect(remote_addr).await?;
        let local_addr = socket.local_addr()?;

        info!("UDP transport auto-bound to {} -> {}", local_addr, remote_addr);

        Ok(Self { socket: Arc::new(socket), remote_addr, local_addr })
    }

    /// Get the UDP socket for this transport
    pub fn socket(&self) -> Arc<UdpSocket> {
        self.socket.clone()
    }

    /// Get the local address
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Get the remote address
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }
}

#[async_trait::async_trait]
impl UnorderedLinkTransport for UdpTransport {
    async fn send_raw(&self, data: &[u8]) -> Result<usize, std::io::Error> {
        // Send raw data via UDP
        match self.socket.send(data).await {
            Ok(bytes_sent) => {
                debug!("UDP sent {} bytes to {}", bytes_sent, self.remote_addr);
                Ok(bytes_sent)
            }
            Err(e) => {
                error!("UDP send failed: {}", e);
                Err(e)
            }
        }
    }

    fn remote_addr(&self) -> String {
        self.remote_addr.to_string()
    }

    async fn is_healthy(&self) -> bool {
        // Simple health check - try to get socket info
        self.socket.local_addr().is_ok()
    }
}

impl fmt::Display for UdpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UdpTransport({}->{})", self.local_addr, self.remote_addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::test;

    #[test]
    async fn test_udp_transport_creation() {
        let transport = UdpTransport::new("127.0.0.1:0".parse().unwrap()).await.unwrap();
        assert!(transport.local_addr().port() > 0);
        assert!(transport.remote_addr().ip().is_unspecified());
    }

    #[test]
    async fn test_udp_transport_connect() {
        let transport = UdpTransport::connect("127.0.0.1:0".parse().unwrap(), "127.0.0.1:8080".parse().unwrap())
            .await
            .unwrap();

        assert_eq!(transport.remote_addr().port(), 8080);
        assert!(transport.is_healthy().await);
    }

    #[test]
    async fn test_udp_transport_display() {
        let transport = UdpTransport::connect("127.0.0.1:0".parse().unwrap(), "127.0.0.1:8080".parse().unwrap())
            .await
            .unwrap();

        let display = format!("{}", transport);
        assert!(display.contains("UdpTransport"));
        assert!(display.contains("127.0.0.1"));
    }

    /// Example usage of UDP transport with aggregation
    #[tokio::test]
    async fn test_udp_transport_basic_functionality() {
        // Test basic transport operations
        let transport = UdpTransport::new("127.0.0.1:0".parse().unwrap()).await.unwrap();

        // Test health check
        assert!(transport.is_healthy().await);

        // Test remote address before connection
        assert_eq!(transport.remote_addr().ip().to_string(), "0.0.0.0");

        // Test trait implementation - send_raw should work with dummy data
        let test_data = b"test data";
        // Note: This will fail because there's no server, but validates the interface
        let result = transport.send_raw(test_data).await;
        assert!(result.is_err()); // Expected to fail without a server
    }
}
