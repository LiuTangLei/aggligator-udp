//! Unordered aggregation task management.
//!
//! This module implements the core unordered aggregation logic without
//! the complexities of ordered delivery and acknowledgments.
//! Each link operates independently for maximum performance.
//!
//! Note: This module provides the core aggregation logic and is
//! transport-agnostic. Actual implementations should be provided
//! by transport-specific crates like `aggligator-transport-udp`,
//! `aggligator-transport-quic`, etc.

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, atomic::{AtomicU64, Ordering}},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{debug, info, warn, error, trace};

use crate::{
    id::LinkId,
    unordered_cfg::{LoadBalanceStrategy, UnorderedCfg},
    unordered_msg::UnorderedLinkMsg,
};

/// Detailed error types for unordered aggregation operations
#[derive(Debug)]
pub enum UnorderedAggError {
    /// No links are available for sending data
    NoLinksAvailable,
    /// No healthy links are available
    NoHealthyLinks { 
        /// Total number of links available
        total_links: usize 
    },
    /// Specific link not found
    LinkNotFound { 
        /// The link ID that was not found
        link_id: LinkId 
    },
    /// Link is unhealthy
    LinkUnhealthy { 
        /// The unhealthy link ID
        link_id: LinkId 
    },
    /// Network I/O error
    NetworkError { 
        /// Optional link ID where error occurred
        link_id: Option<LinkId>, 
        /// The underlying I/O error
        source: std::io::Error 
    },
    /// Control channel closed or send error
    ChannelClosed {
        /// Context for the channel error
        context: String
    },
    /// Control operation timeout or receive error
    TimeoutOrRecvError { 
        /// Description of the operation that timed out or failed
        operation: String 
    },
    /// Internal error with context
    Internal { 
        /// Context information about where the error occurred
        context: String, 
        /// Optional underlying error
        source: Option<Box<dyn std::error::Error + Send + Sync>> 
    },
}

impl fmt::Display for UnorderedAggError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnorderedAggError::NoLinksAvailable => {
                write!(f, "No links are available for sending data")
            }
            UnorderedAggError::NoHealthyLinks { total_links } => {
                write!(f, "No healthy links available out of {} total links", total_links)
            }
            UnorderedAggError::LinkNotFound { link_id } => {
                write!(f, "Link {} not found", link_id)
            }
            UnorderedAggError::LinkUnhealthy { link_id } => {
                write!(f, "Link {} is unhealthy", link_id)
            }
            UnorderedAggError::NetworkError { link_id, source } => {
                match link_id {
                    Some(id) => write!(f, "Network error on link {}: {}", id, source),
                    None => write!(f, "Network error: {}", source),
                }
            }
            UnorderedAggError::ChannelClosed { context } => {
                write!(f, "Control channel operation failed (channel closed or send error): {}", context)
            }
            UnorderedAggError::TimeoutOrRecvError { operation } => {
                write!(f, "Operation '{}' timed out or failed to receive response", operation)
            }
            UnorderedAggError::Internal { context, source } => {
                match source {
                    Some(e) => write!(f, "Internal error in {}: {}", context, e),
                    None => write!(f, "Internal error in {}", context),
                }
            }
        }
    }
}

impl std::error::Error for UnorderedAggError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            UnorderedAggError::NetworkError { source, .. } => Some(source),
            UnorderedAggError::Internal { source: Some(e), .. } => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl From<mpsc::error::SendError<UnorderedControlMsg>> for UnorderedAggError {
    fn from(e: mpsc::error::SendError<UnorderedControlMsg>) -> Self {
        UnorderedAggError::ChannelClosed { context: format!("sending control message: {}", e) }
    }
}

impl From<oneshot::error::RecvError> for UnorderedAggError {
    fn from(e: oneshot::error::RecvError) -> Self {
        UnorderedAggError::TimeoutOrRecvError { operation: format!("waiting for oneshot response: {}", e) }
    }
}

/// Result type for unordered aggregation operations
pub type UnorderedAggResult<T> = Result<T, UnorderedAggError>;

/// Detailed information about a send operation result
#[derive(Debug, Clone)]
pub struct SendResult {
    /// Number of bytes successfully sent
    pub bytes_sent: usize,
    /// Link used for sending
    pub link_id: LinkId,
    /// Time taken for the operation
    pub duration: Duration,
    /// Whether this was a retry operation (for future use)
    pub was_retry: bool,
}

/// Trait for transport-agnostic link operations
#[async_trait::async_trait]
pub trait UnorderedLinkTransport: Send + Sync + std::fmt::Debug + 'static {
    /// Send raw data through this transport link (low-level interface)
    async fn send_raw(&self, data: &[u8]) -> Result<usize, std::io::Error>;
    
    /// Send data with session information (high-level interface)
    /// This will automatically wrap the data in UnorderedLinkMsg::Data
    async fn send(&self, data: &[u8]) -> Result<usize, std::io::Error> {
        // Generate a session ID based on data hash for session consistency
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        data.hash(&mut hasher);
        let session_id = hasher.finish();
        
        // Use send_with_session with auto-generated session ID
        // This ensures all data is properly wrapped in UnorderedLinkMsg::Data
        self.send_with_session(data, LinkId(0), session_id, None).await
    }
    
    /// Send data through aggregation (compatibility method)
    /// This method is used by the aggregation task to send data
    async fn send_data(&self, data: &[u8]) -> Result<usize, std::io::Error> {
        // Always use send() which wraps data in UnorderedLinkMsg::Data
        // This replaces the old direct send_raw() call
        self.send(data).await
    }
    
    /// Send data with full session context (for transparent proxy)
    async fn send_with_session(
        &self,
        data: &[u8],
        link_id: LinkId,
        session_id: u64,
        original_client: Option<std::net::SocketAddr>,
    ) -> Result<usize, std::io::Error> {
        // Create wrapped message
        let msg = UnorderedLinkMsg::Data {
            link_id,
            session_id,
            original_client,
            payload: data.to_vec(),
        };
        
        // Serialize message
        let mut buf = Vec::new();
        msg.write(&mut buf).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?;
        
        // Send serialized message
        self.send_raw(&buf).await
    }
    
    /// Get the remote address as a string for identification
    fn remote_addr(&self) -> String;
    
    /// Check if the link is healthy
    async fn is_healthy(&self) -> bool {
        true // Default implementation
    }
}

/// Unordered link state information (transport-agnostic)
#[derive(Debug, Clone)]
pub struct UnorderedLinkState {
    /// Link identifier
    pub link_id: LinkId,
    /// Transport implementation for this link
    pub transport: Arc<dyn UnorderedLinkTransport>,
    /// Remote address string for display/logging
    pub remote_addr: String,
    /// Last successful communication time
    pub last_seen: Instant,
    /// Number of bytes sent through this link
    pub bytes_sent: Arc<AtomicU64>,
    /// Number of bytes received from this link
    pub bytes_received: Arc<AtomicU64>,
    /// Whether this link is considered healthy
    pub is_healthy: bool,
}

impl UnorderedLinkState {
    /// Create a new unordered link state
    pub fn new(link_id: LinkId, transport: Arc<dyn UnorderedLinkTransport>) -> Self {
        let remote_addr = transport.remote_addr();
        Self {
            link_id,
            transport,
            remote_addr,
            last_seen: Instant::now(),
            bytes_sent: Arc::new(AtomicU64::new(0)),
            bytes_received: Arc::new(AtomicU64::new(0)),
            is_healthy: true,
        }
    }

    /// Update statistics for sent data
    pub fn record_sent(&self, bytes: usize) {
        self.bytes_sent.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Update statistics for received data
    pub fn record_received(&self, bytes: usize) {
        self.bytes_received.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

/// Control messages for unordered aggregation task
#[derive(Debug)]
pub enum UnorderedControlMsg {
    /// Add a new link
    AddLink {
        /// Link identifier
        link_id: LinkId,
        /// Transport implementation
        transport: Arc<dyn UnorderedLinkTransport>,
    },
    /// Remove a link
    RemoveLink { 
        /// Link identifier
        link_id: LinkId 
    },
    /// Update link health status
    UpdateLinkHealth { 
        /// Link identifier
        link_id: LinkId, 
        /// Whether the link is healthy
        is_healthy: bool 
    },
    /// Get aggregation statistics
    GetStats {
        /// Channel to send response
        respond_to: oneshot::Sender<UnorderedAggStats>,
    },
    /// Send data through the aggregation
    SendData {
        /// Data to be sent
        data: Vec<u8>,
        /// Channel to send response
        respond_to: oneshot::Sender<UnorderedAggResult<SendResult>>,
    },
    /// Send data through a specific link
    SendDataViaLink {
        /// Data to be sent
        data: Vec<u8>,
        /// Link identifier
        link_id: LinkId,
        /// Channel to send response
        respond_to: oneshot::Sender<UnorderedAggResult<SendResult>>,
    },
    /// Send data through the aggregation with session information
    SendDataWithSession {
        /// Data to be sent
        data: Vec<u8>,
        /// Session ID for transparent proxy
        session_id: u64,
        /// Original client address for transparent proxy
        original_client: Option<std::net::SocketAddr>,
        /// Channel to send response
        respond_to: oneshot::Sender<UnorderedAggResult<SendResult>>,
    },
    /// Receive data from a link (for forwarding to application)
    ReceiveData {
        /// Received data
        data: Vec<u8>,
        /// Link identifier that received the data
        link_id: LinkId,
    },
    /// Receive data from a link (for forwarding to application) with session support
    ReceiveDataWithSession {
        /// Received data
        data: Vec<u8>,
        /// Link identifier that received the data
        link_id: LinkId,
        /// Session ID for transparent proxy
        session_id: u64,
        /// Original client address for transparent proxy
        original_client: Option<std::net::SocketAddr>,
    },
}

/// Data received from a link with optional session information
#[derive(Debug, Clone)]
pub struct ReceivedData {
    /// The actual data payload
    pub data: Vec<u8>,
    /// Link that received the data
    pub link_id: LinkId,
    /// Session ID for transparent proxy (0 means no session)
    pub session_id: u64,
    /// Original client address for transparent proxy
    pub original_client: Option<std::net::SocketAddr>,
}

impl ReceivedData {
    /// Create new received data without session information
    pub fn new(data: Vec<u8>, link_id: LinkId) -> Self {
        Self {
            data,
            link_id,
            session_id: 0,
            original_client: None,
        }
    }
    
    /// Create new received data with session information
    pub fn with_session(
        data: Vec<u8>, 
        link_id: LinkId, 
        session_id: u64, 
        original_client: Option<std::net::SocketAddr>
    ) -> Self {
        Self {
            data,
            link_id,
            session_id,
            original_client,
        }
    }
}

/// Unordered aggregation statistics
#[derive(Debug, Clone)]
pub struct UnorderedAggStats {
    /// Number of active links
    pub active_links: usize,
    /// Number of healthy links
    pub healthy_links: usize,
    /// Total bytes sent across all links
    pub total_bytes_sent: u64,
    /// Total bytes received across all links
    pub total_bytes_received: u64,
    /// Per-link statistics
    pub link_stats: HashMap<LinkId, UnorderedLinkStats>,
}

/// Statistics for a single unordered link
#[derive(Debug, Clone)]
pub struct UnorderedLinkStats {
    /// Remote address
    pub remote_addr: String,
    /// Whether the link is healthy
    pub is_healthy: bool,
    /// Bytes sent through this link
    pub bytes_sent: u64,
    /// Bytes received from this link
    pub bytes_received: u64,
    /// Last communication time
    pub last_seen: Instant,
    /// Average round-trip time in milliseconds
    pub avg_rtt: f64,
    /// Current bandwidth utilization (bytes/sec)
    pub bandwidth: f64,
    /// Packet loss rate (0.0 - 1.0)
    pub packet_loss_rate: f64,
    /// Number of successful transmissions
    pub successful_sends: u64,
    /// Number of failed transmissions
    pub failed_sends: u64,
}

/// Unordered aggregation task - coordinates multiple unordered links for bandwidth aggregation
#[derive(Debug)]
pub struct UnorderedAggTask {
    /// Configuration
    config: UnorderedCfg,
    /// Channel for sending received data to application
    rx_channel: mpsc::UnboundedSender<ReceivedData>,
    /// Channel for receiving control messages
    control_tx: mpsc::UnboundedSender<UnorderedControlMsg>,
    /// Active links
    links: Arc<RwLock<HashMap<LinkId, UnorderedLinkState>>>,
    /// Load balancer state
    load_balancer: UnorderedLoadBalancer,
}

impl UnorderedAggTask {
    /// Create a new unordered aggregation task
    pub fn new(
        config: UnorderedCfg,
        rx_channel: mpsc::UnboundedSender<ReceivedData>,
        control_tx: mpsc::UnboundedSender<UnorderedControlMsg>,
    ) -> Self {
        Self {
            config: config.clone(),
            rx_channel,
            control_tx,
            links: Arc::new(RwLock::new(HashMap::new())),
            load_balancer: UnorderedLoadBalancer::new(config.load_balance),
        }
    }

    /// Start the aggregation task
    pub async fn run(
        &self,
        mut control_rx: mpsc::UnboundedReceiver<UnorderedControlMsg>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        loop {
            tokio::select! {
                msg = control_rx.recv() => {
                    match msg {
                        Some(UnorderedControlMsg::AddLink { link_id, transport }) => {
                            self.add_link(link_id, transport).await;
                        }
                        Some(UnorderedControlMsg::RemoveLink { link_id }) => {
                            self.remove_link(link_id).await;
                        }
                        Some(UnorderedControlMsg::UpdateLinkHealth { link_id, is_healthy }) => {
                            self.update_link_health(link_id, is_healthy).await;
                        }
                        Some(UnorderedControlMsg::GetStats { respond_to }) => {
                            let stats = self.get_stats().await;
                            let _ = respond_to.send(stats);
                        }
                        Some(UnorderedControlMsg::SendData { data, respond_to }) => {
                            let result = self.send_data(data).await;
                            if let Err(e) = &result {
                                error!("Send data failed from control message: {}", e);
                            }
                            let _ = respond_to.send(result);
                        }
                        Some(UnorderedControlMsg::SendDataViaLink { data, link_id, respond_to }) => {
                            let result = self.send_data_via_link(data, link_id).await;
                            if let Err(e) = &result {
                                error!("Send data via link {} failed from control message: {}", link_id, e);
                            }
                            let _ = respond_to.send(result);
                        }
                        Some(UnorderedControlMsg::ReceiveData { data, link_id }) => {
                            self.handle_incoming_data(data, link_id).await;
                        }
                        Some(UnorderedControlMsg::ReceiveDataWithSession { data, link_id, session_id, original_client }) => {
                            self.handle_incoming_data_with_session(data, link_id, session_id, original_client).await;
                        }
                        Some(UnorderedControlMsg::SendDataWithSession { data, session_id, original_client, respond_to }) => {
                            let result = self.send_data_with_session(data, session_id, original_client).await;
                            if let Err(e) = &result {
                                error!("Send data with session {} failed from control message: {}", session_id, e);
                            }
                            let _ = respond_to.send(result);
                        }
                        None => break,
                    }
                }
            }
        }
        Ok(())
    }

    /// Send data through aggregated links
    pub async fn send(&self, data: Vec<u8>) -> Result<(), std::io::Error> {
        let links = self.links.read().await;
        if links.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "No links available"
            ));
        }

        let link_id = match self.load_balancer.select_link(&links).await {
            Some(id) => id,
            None => return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "No links available for selection"
            )),
        };
        
        if let Some(link_state) = links.get(&link_id) {
            match link_state.transport.send(&data).await {
                Ok(bytes_sent) => {
                    link_state.record_sent(bytes_sent);
                    Ok(())
                }
                Err(e) => {
                    eprintln!("Failed to send data through link {:?}: {}", link_id, e);
                    Err(e)
                }
            }
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Selected link not found"
            ))
        }
    }

    /// Send data through the aggregation with detailed error reporting
    pub async fn send_data(&self, data: Vec<u8>) -> UnorderedAggResult<SendResult> {
        // Generate session ID from data for consistency
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        data.hash(&mut hasher);
        let session_id = hasher.finish();
        
        // Use send_data_with_session for proper data encapsulation
        self.send_data_with_session(data, session_id, None).await
    }

    /// Send data through a specific link
    pub async fn send_data_via_link(
        &self, 
        data: Vec<u8>, 
        link_id: LinkId
    ) -> UnorderedAggResult<SendResult> {
        let start_time = Instant::now();
        let data_len = data.len();
        debug!("Attempting to send {} bytes via specific link {}", data_len, link_id);

        // Get transport and state outside of lock
        let (transport, link_state_clone) = {
            let links = self.links.read().await;
            
            if let Some(link_state) = links.get(&link_id) {
                if !link_state.is_healthy {
                    warn!("Send via link {} failed: Link is unhealthy", link_id);
                    return Err(UnorderedAggError::LinkUnhealthy { link_id });
                }
                info!("Sending {} bytes via specific link {} ({})", data_len, link_id, link_state.remote_addr);
                // Clone the Arc pointers for use outside of the lock
                (link_state.transport.clone(), link_state.clone())
            } else {
                error!("Send via link {} failed: Link not found", link_id);
                return Err(UnorderedAggError::LinkNotFound { link_id });
            }
        }; // Lock is released here
        
        // Perform network IO outside of lock
        match transport.send(&data).await {
            Ok(bytes_sent) => {
                let duration = start_time.elapsed();
                link_state_clone.record_sent(bytes_sent);
                info!("Successfully sent {} bytes via link {} in {:?}", bytes_sent, link_id, duration);
                Ok(SendResult {
                    bytes_sent,
                    link_id,
                    duration,
                    was_retry: false,
                })
            }
            Err(e) => {
                let duration = start_time.elapsed();
                error!("Send via link {} failed after {:?}: {}", link_id, duration, e);
                Err(UnorderedAggError::NetworkError { 
                    link_id: Some(link_id), 
                    source: e 
                })
            }
        }
    }

    /// Send data through the aggregation with session information
    pub async fn send_data_with_session(
        &self, 
        data: Vec<u8>, 
        session_id: u64, 
        original_client: Option<std::net::SocketAddr>
    ) -> UnorderedAggResult<SendResult> {
        let start_time = Instant::now();
        let data_len = data.len();
        debug!("Attempting to send {} bytes with session {} via aggregation", data_len, session_id);

        // Get transport and state outside of lock
        let (transport, link_state_clone, selected_link_id) = {
            let links = self.links.read().await;
            
            trace!("Current links: {} total", links.len());

            if links.is_empty() {
                warn!("Send with session failed: No links available");
                return Err(UnorderedAggError::NoLinksAvailable);
            }

            let healthy_count = links.values().filter(|s| s.is_healthy).count();
            debug!("Available links for session send: {} total, {} healthy", links.len(), healthy_count);

            let selected_link_id = match self.load_balancer.select_link_with_session(&links, session_id).await {
                Some(id) => {
                    debug!("Load balancer selected link {} for session {} (with affinity)", id, session_id);
                    id
                }
                None => {
                    warn!("Send with session failed: Load balancer found no suitable links (total: {}, healthy: {})",
                          links.len(), healthy_count);
                    return Err(UnorderedAggError::NoHealthyLinks { total_links: links.len() });
                }
            };
            
            if let Some(link_state) = links.get(&selected_link_id) {
                if link_state.is_healthy {
                    info!("Sending {} bytes with session {} via link {} ({})", 
                          data_len, session_id, selected_link_id, link_state.remote_addr);
                    // Clone the Arc pointers for use outside of the lock
                    (link_state.transport.clone(), link_state.clone(), selected_link_id)
                } else {
                    warn!("Selected link {} for session send is unhealthy", selected_link_id);
                    return Err(UnorderedAggError::LinkUnhealthy { link_id: selected_link_id });
                }
            } else {
                error!("Selected link {} for session send not found in links map", selected_link_id);
                return Err(UnorderedAggError::LinkNotFound { link_id: selected_link_id });
            }
        }; // Lock is released here
        
        // Perform network IO outside of lock using session-aware sending
        match transport.send_with_session(&data, selected_link_id, session_id, original_client).await {
            Ok(bytes_sent) => {
                let duration = start_time.elapsed();
                // Update statistics using cloned state
                link_state_clone.record_sent(bytes_sent);
                info!("Successfully sent {} bytes with session {} via link {} in {:?}", 
                      bytes_sent, session_id, selected_link_id, duration);
                Ok(SendResult {
                    bytes_sent,
                    link_id: selected_link_id,
                    duration,
                    was_retry: false, // Retries not implemented yet
                })
            }
            Err(e) => {
                let duration = start_time.elapsed();
                error!("Send with session {} via link {} failed after {:?}: {}", 
                       session_id, selected_link_id, duration, e);
                Err(UnorderedAggError::NetworkError { 
                    link_id: Some(selected_link_id), 
                    source: e 
                })
            }
        }
    }

    /// Handle incoming data from a link
    async fn handle_incoming_data(&self, data: Vec<u8>, from_link: LinkId) {
        // Update statistics for received data
        let data_len = data.len();
        {
            let links = self.links.read().await;
            if let Some(link_state) = links.get(&from_link) {
                link_state.record_received(data_len);
            }
        }
        
        // In the unordered aggregation model, we simply forward all received data
        // to the application without any ordering or deduplication
        let received_data = ReceivedData::new(data, from_link);
        if let Err(e) = self.rx_channel.send(received_data) {
            eprintln!("Failed to forward received data: {}", e);
        }
    }

    /// Handle incoming data from a link with session information
    async fn handle_incoming_data_with_session(
        &self, 
        data: Vec<u8>, 
        from_link: LinkId, 
        _session_id: u64, 
        _original_client: Option<std::net::SocketAddr>
    ) {
        // Update statistics for received data
        let data_len = data.len();
        {
            let links = self.links.read().await;
            if let Some(link_state) = links.get(&from_link) {
                link_state.record_received(data_len);
            }
        }
        
        // Forward data with session information for transparent proxy
        let received_data = ReceivedData::with_session(data, from_link, _session_id, _original_client);
        if let Err(e) = self.rx_channel.send(received_data) {
            eprintln!("Failed to forward received data with session: {}", e);
        }
    }

    /// Add a new link to the aggregation
    async fn add_link(&self, link_id: LinkId, transport: Arc<dyn UnorderedLinkTransport>) {
        let link_state = UnorderedLinkState::new(link_id, transport);
        let mut links = self.links.write().await;
        links.insert(link_id, link_state);
        println!("Added unordered link: {}", link_id);
    }

    /// Remove a link from the aggregation
    async fn remove_link(&self, link_id: LinkId) {
        let mut links = self.links.write().await;
        if links.remove(&link_id).is_some() {
            println!("Removed unordered link: {}", link_id);
        }
    }

    /// Update the health status of a link
    async fn update_link_health(&self, link_id: LinkId, is_healthy: bool) {
        let mut links = self.links.write().await;
        if let Some(link_state) = links.get_mut(&link_id) {
            link_state.is_healthy = is_healthy;
            println!("Updated link {} health: {}", link_id, is_healthy);
        }
    }

    /// Get current aggregation statistics
    async fn get_stats(&self) -> UnorderedAggStats {
        let links = self.links.read().await;
        let mut total_bytes_sent = 0;
        let mut total_bytes_received = 0;
        let mut healthy_links = 0;
        let mut link_stats = HashMap::new();

        for (link_id, link_state) in links.iter() {
            let bytes_sent = link_state.bytes_sent.load(Ordering::Relaxed);
            let bytes_received = link_state.bytes_received.load(Ordering::Relaxed);
            
            total_bytes_sent += bytes_sent;
            total_bytes_received += bytes_received;
            
            if link_state.is_healthy {
                healthy_links += 1;
            }

            link_stats.insert(*link_id, UnorderedLinkStats {
                remote_addr: link_state.remote_addr.clone(),
                is_healthy: link_state.is_healthy,
                bytes_sent,
                bytes_received,
                last_seen: link_state.last_seen,
                avg_rtt: 0.0, // Will be populated from load balancer metrics
                bandwidth: 0.0, // Will be populated from load balancer metrics  
                packet_loss_rate: 0.0, // Will be calculated from send stats
                successful_sends: 0, // To be tracked in future enhancement
                failed_sends: 0, // To be tracked in future enhancement
            });
        }

        UnorderedAggStats {
            active_links: links.len(),
            healthy_links,
            total_bytes_sent,
            total_bytes_received,
            link_stats,
        }
    }
}

/// Load balancer for unordered aggregation
#[derive(Debug)]
pub struct UnorderedLoadBalancer {
    strategy: LoadBalanceStrategy,
    round_robin_index: std::sync::atomic::AtomicUsize,
    /// Link performance tracking for weighted algorithms
    link_metrics: Arc<RwLock<HashMap<LinkId, LinkMetrics>>>,
    /// Session to link mapping for session affinity
    session_affinity: Arc<RwLock<HashMap<u64, LinkId>>>,
}

/// Performance metrics for each link
#[derive(Debug, Clone)]
struct LinkMetrics {
    /// Average round-trip time in milliseconds
    avg_rtt: f64,
    /// Bandwidth utilization (bytes/sec)
    bandwidth: f64,
    /// Last update timestamp
    last_update: Instant,
}

impl UnorderedLoadBalancer {
    /// Create a new load balancer
    pub fn new(strategy: LoadBalanceStrategy) -> Self {
        Self {
            strategy,
            round_robin_index: std::sync::atomic::AtomicUsize::new(0),
            link_metrics: Arc::new(RwLock::new(HashMap::new())),
            session_affinity: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Update link performance metrics
    pub async fn update_link_metrics(&self, link_id: LinkId, rtt: Duration, bytes_per_sec: f64) {
        let mut metrics = self.link_metrics.write().await;
        metrics.insert(link_id, LinkMetrics {
            avg_rtt: rtt.as_millis() as f64,
            bandwidth: bytes_per_sec,
            last_update: Instant::now(),
        });
    }

    /// Select a link for sending data
    pub async fn select_link(&self, links: &HashMap<LinkId, UnorderedLinkState>) -> Option<LinkId> {
        let healthy_links: Vec<_> = links
            .iter()
            .filter(|(_, state)| state.is_healthy)
            .collect();

        if healthy_links.is_empty() {
            // Fallback to any available link if no healthy ones
            return links.keys().next().copied();
        }

        match self.strategy {
            LoadBalanceStrategy::RoundRobin => {
                let index = self.round_robin_index
                    .fetch_add(1, Ordering::Relaxed) % healthy_links.len();
                Some(*healthy_links[index].0)
            }
            LoadBalanceStrategy::PacketRoundRobin => {
                // Packet-level round-robin without session affinity
                let index = self.round_robin_index
                    .fetch_add(1, Ordering::Relaxed) % healthy_links.len();
                Some(*healthy_links[index].0)
            }
            LoadBalanceStrategy::Random => {
                use rand::Rng;
                let mut rng = rand::rng();
                let index = rng.random_range(0..healthy_links.len());
                Some(*healthy_links[index].0)
            }
            LoadBalanceStrategy::WeightedByBandwidth => {
                // Use bandwidth-based weighted selection
                let metrics = self.link_metrics.read().await;
                let mut best_link = *healthy_links[0].0;
                let mut best_bandwidth = 0.0;
                
                for (link_id, _) in healthy_links {
                    if let Some(metric) = metrics.get(link_id) {
                        if metric.bandwidth > best_bandwidth {
                            best_bandwidth = metric.bandwidth;
                            best_link = *link_id;
                        }
                    }
                }
                Some(best_link)
            }
            LoadBalanceStrategy::FastestFirst => {
                // Use latency-based selection (lowest RTT wins)
                let metrics = self.link_metrics.read().await;
                let mut best_link = *healthy_links[0].0;
                let mut best_rtt = f64::MAX;
                
                for (link_id, _) in healthy_links {
                    if let Some(metric) = metrics.get(link_id) {
                        if metric.avg_rtt < best_rtt {
                            best_rtt = metric.avg_rtt;
                            best_link = *link_id;
                        }
                    }
                }
                Some(best_link)
            }
        }
    }

    /// Select a link for sending data with session affinity
    pub async fn select_link_with_session(&self, links: &HashMap<LinkId, UnorderedLinkState>, session_id: u64) -> Option<LinkId> {
        // For PacketRoundRobin strategy, ignore session affinity and use packet-level load balancing
        if matches!(self.strategy, LoadBalanceStrategy::PacketRoundRobin) {
            return self.select_link(links).await;
        }

        // For other strategies, use session affinity
        // Check if we have session affinity
        {
            let affinity = self.session_affinity.read().await;
            if let Some(&link_id) = affinity.get(&session_id) {
                // Verify the link is still healthy
                if let Some(link_state) = links.get(&link_id) {
                    if link_state.is_healthy {
                        return Some(link_id);
                    }
                }
                // Link is unhealthy, we'll select a new one below
            }
        }

        // No existing affinity or link is unhealthy, select a new link
        if let Some(selected_link) = self.select_link(links).await {
            // Store session affinity
            let mut affinity = self.session_affinity.write().await;
            affinity.insert(session_id, selected_link);
            Some(selected_link)
        } else {
            None
        }
    }

    /// Clean up old session affinities
    pub async fn cleanup_old_sessions(&self, max_age: Duration) {
        let mut affinity = self.session_affinity.write().await;
        let _cutoff = Instant::now() - max_age;
        // Note: We'd need to track session last-used time for proper cleanup
        // For now, just limit the size
        if affinity.len() > 1000 {
            affinity.clear();
        }
    }
}

/// Unordered aggregation handle for external API
pub struct UnorderedAggHandle {
    /// Control channel for sending commands
    control_tx: mpsc::UnboundedSender<UnorderedControlMsg>,
    /// Data channel for receiving incoming data  
    rx_channel: mpsc::UnboundedReceiver<ReceivedData>,
}

impl UnorderedAggHandle {
    /// Create a new handle
    pub fn new(
        control_tx: mpsc::UnboundedSender<UnorderedControlMsg>,
        rx_channel: mpsc::UnboundedReceiver<ReceivedData>,
    ) -> Self {
        Self {
            control_tx,
            rx_channel,
        }
    }

    /// Add a new link to the aggregation
    pub async fn add_link(
        &self,
        link_id: LinkId,
        transport: Arc<dyn UnorderedLinkTransport>,
    ) -> Result<(), mpsc::error::SendError<UnorderedControlMsg>> {
        self.control_tx.send(UnorderedControlMsg::AddLink { link_id, transport })
    }

    /// Remove a link from the aggregation
    pub async fn remove_link(&self, link_id: LinkId) -> Result<(), mpsc::error::SendError<UnorderedControlMsg>> {
        self.control_tx.send(UnorderedControlMsg::RemoveLink { link_id })
    }

    /// Get aggregation statistics
    pub async fn get_stats(&self) -> Result<UnorderedAggStats, Box<dyn std::error::Error + Send + Sync>> {
        let (tx, rx) = oneshot::channel();
        self.control_tx.send(UnorderedControlMsg::GetStats { respond_to: tx })?;
        let stats = rx.await?;
        Ok(stats)
    }

    /// Send data through the aggregation
    pub async fn send(&self, data: Vec<u8>) -> UnorderedAggResult<SendResult> {
        let (tx, rx) = oneshot::channel();
        self.control_tx.send(UnorderedControlMsg::SendData { 
            data, 
            respond_to: tx 
        }).map_err(|e| UnorderedAggError::ChannelClosed { context: format!("sending SendData control message: {}", e) })?;
        
        // Wait for send completion and propagate the result
        rx.await.map_err(|e| UnorderedAggError::TimeoutOrRecvError { operation: format!("waiting for SendData response: {}", e) })?
    }

    /// Send data through a specific link
    pub async fn send_via_link(
        &self, 
        data: Vec<u8>, 
        link_id: LinkId
    ) -> UnorderedAggResult<SendResult> {
        let (tx, rx) = oneshot::channel();
        self.control_tx.send(UnorderedControlMsg::SendDataViaLink { 
            data, 
            link_id,
            respond_to: tx 
        }).map_err(|e| UnorderedAggError::ChannelClosed { context: format!("sending SendDataViaLink control message: {}", e) })?;
        
        // Wait for send completion and propagate the result
        rx.await.map_err(|e| UnorderedAggError::TimeoutOrRecvError { operation: format!("waiting for SendDataViaLink response: {}", e) })?
    }

    /// Send data through the aggregation with session information
    pub async fn send_with_session(
        &self, 
        data: Vec<u8>, 
        session_id: u64, 
        original_client: Option<std::net::SocketAddr>
    ) -> UnorderedAggResult<SendResult> {
        let (tx, rx) = oneshot::channel();
        self.control_tx.send(UnorderedControlMsg::SendDataWithSession { 
            data, 
            session_id,
            original_client,
            respond_to: tx 
        }).map_err(|e| UnorderedAggError::ChannelClosed { context: format!("sending SendDataWithSession control message: {}", e) })?;
        
        // Wait for send completion and propagate the result
        rx.await.map_err(|e| UnorderedAggError::TimeoutOrRecvError { operation: format!("waiting for SendDataWithSession response: {}", e) })?
    }

    /// Receive data from any link (returns full ReceivedData with session info)
    pub async fn recv(&mut self) -> Option<ReceivedData> {
        self.rx_channel.recv().await
    }
    
    /// Receive data from any link (legacy method for backward compatibility)
    pub async fn recv_legacy(&mut self) -> Option<(Vec<u8>, LinkId)> {
        match self.rx_channel.recv().await {
            Some(received_data) => Some((received_data.data, received_data.link_id)),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock transport for testing
    #[derive(Debug)]
    struct MockTransport {
        remote_addr: String,
        send_count: AtomicUsize,
    }

    impl MockTransport {
        fn new(addr: &str) -> Self {
            Self {
                remote_addr: addr.to_string(),
                send_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl UnorderedLinkTransport for MockTransport {
        async fn send_raw(&self, data: &[u8]) -> Result<usize, std::io::Error> {
            self.send_count.fetch_add(1, Ordering::Relaxed);
            Ok(data.len())
        }

        fn remote_addr(&self) -> String {
            self.remote_addr.clone()
        }
    }

    #[tokio::test]
    async fn test_unordered_link_state_creation() {
        let transport = Arc::new(MockTransport::new("127.0.0.1:8080"));
        let link_state = UnorderedLinkState::new(LinkId(1), transport);
        
        assert_eq!(link_state.link_id, LinkId(1));
        assert_eq!(link_state.remote_addr, "127.0.0.1:8080");
        assert!(link_state.is_healthy);
    }

    #[tokio::test]
    async fn test_load_balancer_round_robin() {
        let lb = UnorderedLoadBalancer::new(LoadBalanceStrategy::RoundRobin);
        let mut links = HashMap::new();
        
        let transport1 = Arc::new(MockTransport::new("127.0.0.1:8080"));
        let transport2 = Arc::new(MockTransport::new("127.0.0.1:8081"));
        
        links.insert(LinkId(1), UnorderedLinkState::new(LinkId(1), transport1));
        links.insert(LinkId(2), UnorderedLinkState::new(LinkId(2), transport2));

        // Test round-robin behavior
        let first = lb.select_link(&links).await;
        let second = lb.select_link(&links).await;
        let third = lb.select_link(&links).await;
        
        assert_ne!(first, second);
        assert_eq!(first, third); // Should cycle back
    }

    #[tokio::test]
    async fn test_aggregation_task_creation() {
        let config = UnorderedCfg::default();
        let (tx_rx, _rx_rx) = mpsc::unbounded_channel();
        let (tx_control, _rx_control) = mpsc::unbounded_channel();
        
        let task = UnorderedAggTask::new(config, tx_rx, tx_control);
        let stats = task.get_stats().await;
        
        assert_eq!(stats.active_links, 0);
        assert_eq!(stats.healthy_links, 0);
    }
}
