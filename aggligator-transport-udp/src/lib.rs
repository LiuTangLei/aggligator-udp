//! UDP transport implementation for Aggligator unordered aggregation.
//!
//! This module provides a concrete implementation of the `UnorderedLinkTransport`
//! trait using UDP sockets, enabling efficient unordered packet aggregation
//! over UDP connections.

use std::{
    collections::HashMap,
    fmt,
    io,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use bytes::{Bytes, BytesMut};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, RwLock},
    task::AbortHandle,
    time::{self, sleep},
};
use tracing::{debug, error, info, warn};

use aggligator::{
    id::LinkId,
    UnorderedLinkTransport,
    unordered_task::{UnorderedControlMsg, UnorderedAggTask, ReceivedData},
    unordered_cfg::UnorderedCfg,
    unordered_msg::UnorderedLinkMsg,
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
    async fn send_raw(&self, data: &[u8]) -> Result<usize, io::Error> {
        if self.remote_addr.port() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "UDP transport not connected to a remote address"
            ));
        }

        // If socket is connected, use send(). Otherwise, use send_to().
        // On tokio, after connect(), send_to() will error on macOS.
        let local_addr = self.socket.local_addr();
        let peer_addr = self.socket.peer_addr();
        match (local_addr, peer_addr) {
            (Ok(_), Ok(_)) => {
                // Connected socket, use send()
                let result = time::timeout(self.send_timeout, self.socket.send(data)).await;
                match result {
                    Ok(Ok(bytes_sent)) => {
                        *self.last_send_time.write().await = Instant::now();
                        Ok(bytes_sent)
                    }
                    Ok(Err(e)) => {
                        error!("Failed to send UDP data (connected): {}", e);
                        Err(e)
                    }
                    Err(_) => {
                        error!("UDP send (connected) timed out after {:?}", self.send_timeout);
                        Err(io::Error::new(io::ErrorKind::TimedOut, "UDP send timeout"))
                    }
                }
            }
            _ => {
                // Not connected, fallback to send_to
                self.send_to(data, self.remote_addr).await
            }
        }
    }

    /// Send data with full session context (implements multi-link aggregation encapsulation)
    async fn send_with_session(
        &self,
        data: &[u8],
        link_id: LinkId,
        session_id: u64,
        original_client: Option<std::net::SocketAddr>,
    ) -> Result<usize, std::io::Error> {
        // Create wrapped message for multi-link aggregation
        let msg = UnorderedLinkMsg::Data {
            link_id,
            session_id,
            original_client,
            payload: data.to_vec(),
        };
        
        // Serialize message
        let mut buf = Vec::new();
        msg.write(&mut buf).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, 
                               format!("Failed to serialize UnorderedLinkMsg::Data: {}", e))
        })?;
        
        debug!("UDP transport sending {} bytes wrapped in UnorderedLinkMsg::Data (session: {}, link: {})", 
               data.len(), session_id, link_id);
        
        // Send serialized message through raw UDP
        self.send_raw(&buf).await
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

/// UDP aggregation manager for multiple links with proper task management
pub struct UdpAggregationManager {
    /// Control channel sender
    control_sender: mpsc::UnboundedSender<UnorderedControlMsg>,
    /// Channel for receiving data from remote
    rx_receiver: Arc<RwLock<Option<mpsc::UnboundedReceiver<ReceivedData>>>>,
    /// Track running tasks for proper cleanup
    link_tasks: Arc<RwLock<HashMap<LinkId, (AbortHandle, mpsc::UnboundedSender<()>)>>>,
}

impl UdpAggregationManager {
    /// Create a new UDP aggregation manager
    pub async fn new(config: UnorderedCfg) -> io::Result<Self> {
        let (control_sender, control_receiver) = mpsc::unbounded_channel();
        let (rx_sender, rx_receiver) = mpsc::unbounded_channel();
        
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
            rx_receiver: Arc::new(RwLock::new(Some(rx_receiver))),
            link_tasks: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Add a UDP transport as a new link and start receiving data from it
    pub async fn add_udp_link(&self, link_id: LinkId, transport: UdpTransport) -> io::Result<()> {
        let transport_arc = Arc::new(transport);
        
        // Start a receiver task for this transport
        let socket = transport_arc.socket();
        let control_sender = self.control_sender.clone();
        let is_connected = socket.peer_addr().is_ok();
        
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            loop {
                let receive_result = if is_connected {
                    // For connected sockets, use recv()
                    socket.recv(&mut buf).await.map(|len| (len, socket.peer_addr().unwrap()))
                } else {
                    // For unconnected sockets, use recv_from()
                    socket.recv_from(&mut buf).await
                };

                match receive_result {
                    Ok((len, _from)) => {
                        let data = buf[..len].to_vec();
                        debug!("Received {} bytes on link {}", len, link_id);
                        
                        // Try to parse as UnorderedLinkMsg::Data, fallback to raw data
                        let receive_msg = match UnorderedLinkMsg::read(&mut &data[..]) {
                            Ok(UnorderedLinkMsg::Data { link_id: msg_link_id, session_id, original_client, payload }) => {
                                debug!("Received wrapped data: link_id={}, session_id={}, payload_len={}", 
                                       msg_link_id, session_id, payload.len());
                                // Use session information if available
                                if session_id != 0 || original_client.is_some() {
                                    UnorderedControlMsg::ReceiveDataWithSession {
                                        data: payload,
                                        link_id: msg_link_id,
                                        session_id,
                                        original_client,
                                    }
                                } else {
                                    UnorderedControlMsg::ReceiveData {
                                        data: payload,
                                        link_id: msg_link_id,
                                    }
                                }
                            }
                            Ok(_other_msg) => {
                                debug!("Received non-data message, ignoring");
                                continue; // Skip non-data messages
                            }
                            Err(_) => {
                                // Failed to parse as wrapped message, treat as raw data
                                debug!("Failed to parse as wrapped message, treating as raw data");
                                UnorderedControlMsg::ReceiveData {
                                    data,
                                    link_id,
                                }
                            }
                        };
                        
                        if control_sender.send(receive_msg).is_err() {
                            error!("Failed to forward received data from link {}", link_id);
                            break;
                        }
                    }
                    Err(e) => {
                        error!("Error receiving data on link {}: {}", link_id, e);
                        // Could implement reconnection logic here
                        sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });
        
        let add_msg = UnorderedControlMsg::AddLink {
            link_id,
            transport: transport_arc.clone(),
        };
        
        self.control_sender.send(add_msg).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "Aggregation task closed")
        })?;
        
        // Create a shutdown channel for this link
        let (health_shutdown_tx, mut health_shutdown_rx) = mpsc::unbounded_channel::<()>();
        
        // Track the task for this link
        let task_handle = tokio::task::spawn({
            async move {
                // Keep the task alive while the link is active
                loop {
                    tokio::select! {
                        _ = health_shutdown_rx.recv() => {
                            debug!("Health check task for link {} shutting down", link_id);
                            break;
                        }
                        _ = sleep(Duration::from_secs(60)) => {
                            // Continue health check loop
                        }
                    }
                }
            }
        });
        
        let abort_handle = task_handle.abort_handle();
        self.link_tasks.write().await.insert(link_id, (abort_handle, health_shutdown_tx));
        
        info!("Added UDP link {} to aggregation", link_id);
        Ok(())
    }

    /// Take the receiver for incoming data
    pub async fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<ReceivedData>> {
        self.rx_receiver.write().await.take()
    }

    /// Start receiving data from the aggregation and forward to a callback
    pub async fn start_receiving<F>(&self, mut callback: F) -> io::Result<()>
    where
        F: FnMut(Vec<u8>, LinkId) + Send + 'static,
    {
        if let Some(mut receiver) = self.take_receiver().await {
            tokio::spawn(async move {
                while let Some(received_data) = receiver.recv().await {
                    debug!("Received {} bytes from aggregation on link {}", received_data.data.len(), received_data.link_id);
                    callback(received_data.data, received_data.link_id);
                }
                debug!("Aggregation receiver task finished");
            });
        }
        Ok(())
    }

    /// Get a receiver for manually handling received data
    pub async fn get_receiver(&self) -> Option<mpsc::UnboundedReceiver<ReceivedData>> {
        self.take_receiver().await
    }

    /// Remove a link from the aggregation
    pub async fn remove_link(&self, link_id: LinkId) -> io::Result<()> {
        // Stop and remove the task for this link
        if let Some((abort_handle, health_shutdown_tx)) = self.link_tasks.write().await.remove(&link_id) {
            // First try graceful shutdown
            let _ = health_shutdown_tx.send(());
            // Then abort if needed
            abort_handle.abort();
        }
        
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
        
        let result = respond_rx.await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "Failed to receive response")
        })?;
        
        // Convert UnorderedAggResult<SendResult> to io::Result<usize>
        match result {
            Ok(send_result) => Ok(send_result.bytes_sent),
            Err(agg_error) => {
                // Convert UnorderedAggError to io::Error
                let io_error = match agg_error {
                    aggligator::unordered_task::UnorderedAggError::NoLinksAvailable => {
                        io::Error::new(io::ErrorKind::NotConnected, "No links available")
                    }
                    aggligator::unordered_task::UnorderedAggError::NoHealthyLinks { .. } => {
                        io::Error::new(io::ErrorKind::NotConnected, "No healthy links available")
                    }
                    aggligator::unordered_task::UnorderedAggError::LinkNotFound { .. } => {
                        io::Error::new(io::ErrorKind::NotFound, "Link not found")
                    }
                    aggligator::unordered_task::UnorderedAggError::LinkUnhealthy { .. } => {
                        io::Error::new(io::ErrorKind::NotConnected, "Link unhealthy")
                    }
                    aggligator::unordered_task::UnorderedAggError::NetworkError { source, .. } => source,
                    aggligator::unordered_task::UnorderedAggError::ChannelClosed { .. } => {
                        io::Error::new(io::ErrorKind::BrokenPipe, "Control channel closed")
                    }
                    aggligator::unordered_task::UnorderedAggError::TimeoutOrRecvError { .. } => {
                        io::Error::new(io::ErrorKind::TimedOut, "Operation timed out")
                    }
                    aggligator::unordered_task::UnorderedAggError::Internal { context, .. } => {
                        io::Error::new(io::ErrorKind::Other, format!("Internal error: {}", context))
                    }
                };
                Err(io_error)
            }
        }
    }

    /// Send data through the aggregation with session information for multi-link affinity
    pub async fn send_data_with_session(
        &self, 
        data: Vec<u8>, 
        session_id: u64, 
        original_client: Option<std::net::SocketAddr>
    ) -> io::Result<aggligator::unordered_task::SendResult> {
        let (respond_tx, respond_rx) = tokio::sync::oneshot::channel();
        
        let send_msg = UnorderedControlMsg::SendDataWithSession {
            data,
            session_id,
            original_client,
            respond_to: respond_tx,
        };
        
        self.control_sender.send(send_msg).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "Aggregation task closed")
        })?;
        
        let result = respond_rx.await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "Failed to receive response")
        })?;
        
        // Convert UnorderedAggResult<SendResult> to io::Result<SendResult>
        match result {
            Ok(send_result) => Ok(send_result),
            Err(agg_error) => {
                // Convert UnorderedAggError to io::Error
                let io_error = match agg_error {
                    aggligator::unordered_task::UnorderedAggError::NoLinksAvailable => {
                        io::Error::new(io::ErrorKind::NotConnected, "No links available")
                    }
                    aggligator::unordered_task::UnorderedAggError::NoHealthyLinks { .. } => {
                        io::Error::new(io::ErrorKind::NotConnected, "No healthy links available")
                    }
                    aggligator::unordered_task::UnorderedAggError::LinkNotFound { .. } => {
                        io::Error::new(io::ErrorKind::NotFound, "Link not found")
                    }
                    aggligator::unordered_task::UnorderedAggError::LinkUnhealthy { .. } => {
                        io::Error::new(io::ErrorKind::NotConnected, "Link unhealthy")
                    }
                    aggligator::unordered_task::UnorderedAggError::NetworkError { source, .. } => source,
                    aggligator::unordered_task::UnorderedAggError::ChannelClosed { .. } => {
                        io::Error::new(io::ErrorKind::BrokenPipe, "Control channel closed")
                    }
                    aggligator::unordered_task::UnorderedAggError::TimeoutOrRecvError { .. } => {
                        io::Error::new(io::ErrorKind::TimedOut, "Operation timed out")
                    }
                    aggligator::unordered_task::UnorderedAggError::Internal { context, .. } => {
                        io::Error::new(io::ErrorKind::Other, format!("Internal error: {}", context))
                    }
                };
                Err(io_error)
            }
        }
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
