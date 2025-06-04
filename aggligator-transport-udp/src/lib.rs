//! UDP transport implementation for Aggligator unordered aggregation.
//!
//! This module provides a concrete implementation of the `UnorderedLinkTransport`
//! trait using UDP sockets, enabling efficient unordered packet aggregation
//! over UDP connections.

use std::{
    fmt,
    io,
    net::SocketAddr,
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    time::{Duration, Instant},
};
use bytes::{Bytes, BytesMut};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, RwLock},
    time,
};
use tracing::{debug, error, info, warn};

use aggligator::{
    id::LinkId,
    unordered_task::{UnorderedLinkTransport, UnorderedControlMsg, UnorderedAggTask},
    unordered_cfg::UnorderedCfg,
};

/// UDP transport implementation for unordered aggregation
#[derive(Debug)]
pub struct UdpTransport {
    /// UDP socket for communication
    socket: Arc<UdpSocket>,
    /// Remote address for this transport
    remote_addr: SocketAddr,
    /// Local address bound to the socket
    local_addr: SocketAddr,
    /// Health status of this transport
    is_healthy: Arc<AtomicBool>,
    /// Last successful send time
    last_send_time: Arc<RwLock<Instant>>,
    /// Last successful receive time
    last_recv_time: Arc<RwLock<Instant>>,
    /// Send timeout duration
    send_timeout: Duration,
    /// Health check interval
    health_check_interval: Duration,
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
            is_healthy: Arc::new(AtomicBool::new(true)),
            last_send_time: Arc::new(RwLock::new(Instant::now())),
            last_recv_time: Arc::new(RwLock::new(Instant::now())),
            send_timeout: Duration::from_secs(5),
            health_check_interval: Duration::from_secs(10),
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

    /// Get the UDP socket for this transport
    pub fn socket(&self) -> Arc<UdpSocket> {
        self.socket.clone()
    }

    /// Get the local address
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Set send timeout
    pub fn set_send_timeout(&mut self, timeout: Duration) {
        self.send_timeout = timeout;
    }

    /// Set health check interval
    pub fn set_health_check_interval(&mut self, interval: Duration) {
        self.health_check_interval = interval;
    }

    /// Start a background health check task
    pub fn start_health_check(&self) -> mpsc::UnboundedSender<()> {
        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel();
        let is_healthy = self.is_healthy.clone();
        let last_send_time = self.last_send_time.clone();
        let last_recv_time = self.last_recv_time.clone();
        let interval = self.health_check_interval;
        let remote_addr = self.remote_addr;

        tokio::spawn(async move {
            let mut health_check_interval = time::interval(interval);
            
            loop {
                tokio::select! {
                    _ = health_check_interval.tick() => {
                        let now = Instant::now();
                        let last_send = *last_send_time.read().await;
                        let last_recv = *last_recv_time.read().await;
                        
                        // Consider link unhealthy if no activity for 3x the check interval
                        let timeout = interval * 3;
                        let is_send_stale = now.duration_since(last_send) > timeout;
                        let is_recv_stale = now.duration_since(last_recv) > timeout;
                        
                        let new_health_status = !(is_send_stale && is_recv_stale);
                        let old_health_status = is_healthy.load(Ordering::Relaxed);
                        
                        if new_health_status != old_health_status {
                            is_healthy.store(new_health_status, Ordering::Relaxed);
                            if new_health_status {
                                info!("UDP transport to {} recovered", remote_addr);
                            } else {
                                warn!("UDP transport to {} marked unhealthy", remote_addr);
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        debug!("Health check task for {} shutting down", remote_addr);
                        break;
                    }
                }
            }
        });

        shutdown_tx
    }

    /// Receive data from the socket
    pub async fn recv(&self) -> io::Result<(Bytes, SocketAddr)> {
        let mut buf = BytesMut::with_capacity(65536);
        buf.resize(65536, 0);
        
        let (len, addr) = self.socket.recv_from(&mut buf).await?;
        buf.truncate(len);
        
        // Update receive time
        *self.last_recv_time.write().await = Instant::now();
        
        Ok((buf.freeze(), addr))
    }

    /// Send data to a specific address
    pub async fn send_to(&self, data: &[u8], addr: SocketAddr) -> io::Result<usize> {
        let result = time::timeout(self.send_timeout, self.socket.send_to(data, addr)).await;
        
        match result {
            Ok(Ok(bytes_sent)) => {
                // Update send time
                *self.last_send_time.write().await = Instant::now();
                Ok(bytes_sent)
            }
            Ok(Err(e)) => {
                error!("Failed to send UDP data to {}: {}", addr, e);
                Err(e)
            }
            Err(_) => {
                error!("UDP send to {} timed out after {:?}", addr, self.send_timeout);
                Err(io::Error::new(io::ErrorKind::TimedOut, "UDP send timeout"))
            }
        }
    }
}

#[async_trait::async_trait]
impl UnorderedLinkTransport for UdpTransport {
    async fn send(&self, data: &[u8]) -> Result<usize, io::Error> {
        if self.remote_addr.port() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "UDP transport not connected to a remote address"
            ));
        }
        
        self.send_to(data, self.remote_addr).await
    }

    fn remote_addr(&self) -> String {
        self.remote_addr.to_string()
    }

    async fn is_healthy(&self) -> bool {
        self.is_healthy.load(Ordering::Relaxed)
    }
}

impl fmt::Display for UdpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UdpTransport({}->{})", self.local_addr, self.remote_addr)
    }
}

/// UDP aggregation manager for multiple links
pub struct UdpAggregationManager {
    /// Control channel sender
    control_sender: mpsc::UnboundedSender<UnorderedControlMsg>,
}

impl UdpAggregationManager {
    /// Create a new UDP aggregation manager
    pub async fn new(config: UnorderedCfg) -> io::Result<Self> {
        let (control_sender, control_receiver) = mpsc::unbounded_channel();
        let (rx_sender, _rx_receiver) = mpsc::unbounded_channel();
        
        // Create the aggregation task
        let agg_task = UnorderedAggTask::new(config, rx_sender, control_sender.clone());
        
        // Start the task in the background
        tokio::spawn(async move {
            if let Err(e) = agg_task.run(control_receiver).await {
                error!("Aggregation task failed: {}", e);
            }
        });
        
        Ok(Self {
            control_sender,
        })
    }

    /// Add a UDP transport as a new link
    pub async fn add_udp_link(&self, link_id: LinkId, transport: UdpTransport) -> io::Result<()> {
        let transport_arc = Arc::new(transport);
        
        let add_msg = UnorderedControlMsg::AddLink {
            link_id,
            transport: transport_arc,
        };
        
        self.control_sender.send(add_msg).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "Aggregation task closed")
        })?;
        
        info!("Added UDP link {} to aggregation", link_id);
        Ok(())
    }

    /// Remove a link from the aggregation
    pub async fn remove_link(&self, link_id: LinkId) -> io::Result<()> {
        let remove_msg = UnorderedControlMsg::RemoveLink { link_id };
        
        self.control_sender.send(remove_msg).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "Aggregation task closed")
        })?;
        
        info!("Removed link {} from aggregation", link_id);
        Ok(())
    }

    /// Send data through the aggregation
    pub async fn send_data(&self, data: Vec<u8>) -> io::Result<usize> {
        let (respond_tx, respond_rx) = tokio::sync::oneshot::channel();
        
        let send_msg = UnorderedControlMsg::SendData {
            data,
            respond_to: respond_tx,
        };
        
        self.control_sender.send(send_msg).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "Aggregation task closed")
        })?;
        
        respond_rx.await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "Failed to receive response")
        })
    }

    /// Get aggregation statistics
    pub async fn get_stats(&self) -> io::Result<aggligator::unordered_task::UnorderedAggStats> {
        let (respond_tx, respond_rx) = tokio::sync::oneshot::channel();
        
        let stats_msg = UnorderedControlMsg::GetStats {
            respond_to: respond_tx,
        };
        
        self.control_sender.send(stats_msg).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "Aggregation task closed")
        })?;
        
        respond_rx.await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "Failed to receive response")
        })
    }

    /// Get the control channel sender for direct control
    pub fn control_sender(&self) -> mpsc::UnboundedSender<UnorderedControlMsg> {
        self.control_sender.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[tokio::test]
    async fn test_udp_transport_creation() {
        let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let transport = UdpTransport::new(local_addr).await;
        assert!(transport.is_ok());
        
        let transport = transport.unwrap();
        assert!(transport.local_addr.port() > 0);
        assert!(transport.is_healthy().await);
    }

    #[tokio::test]
    async fn test_udp_transport_connection() {
        let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let remote_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345);
        
        let mut transport = UdpTransport::new(local_addr).await.unwrap();
        let result = transport.connect_to(remote_addr).await;
        assert!(result.is_ok());
        assert_eq!(transport.remote_addr, remote_addr);
    }

    #[tokio::test]
    async fn test_udp_aggregation_manager() {
        let config = UnorderedCfg::default();
        let manager = UdpAggregationManager::new(config).await;
        assert!(manager.is_ok());
    }

    #[tokio::test]
    async fn test_udp_link_management() {
        let config = UnorderedCfg::default();
        let manager = UdpAggregationManager::new(config).await.unwrap();
        
        let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let remote_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345);
        let transport = UdpTransport::connect(local_addr, remote_addr).await.unwrap();
        
        let link_id = LinkId(1u128);
        let result = manager.add_udp_link(link_id, transport).await;
        assert!(result.is_ok());
        
        // Get stats to verify link was added
        let stats = manager.get_stats().await.unwrap();
        assert_eq!(stats.active_links, 1);
        
        // Remove the link
        let result = manager.remove_link(link_id).await;
        assert!(result.is_ok());
    }
}
