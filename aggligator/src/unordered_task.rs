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
    collections::{HashMap, VecDeque},
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{debug, error, info, trace, warn};

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
        total_links: usize,
    },
    /// Specific link not found
    LinkNotFound {
        /// The link ID that was not found
        link_id: LinkId,
    },
    /// Link is unhealthy
    LinkUnhealthy {
        /// The unhealthy link ID
        link_id: LinkId,
    },
    /// Network I/O error
    NetworkError {
        /// Optional link ID where error occurred
        link_id: Option<LinkId>,
        /// The underlying I/O error
        source: std::io::Error,
    },
    /// Control channel closed or send error
    ChannelClosed {
        /// Context for the channel error
        context: String,
    },
    /// Control operation timeout or receive error
    TimeoutOrRecvError {
        /// Description of the operation that timed out or failed
        operation: String,
    },
    /// Internal error with context
    Internal {
        /// Context information about where the error occurred
        context: String,
        /// Optional underlying error
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
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
            UnorderedAggError::NetworkError { link_id, source } => match link_id {
                Some(id) => write!(f, "Network error on link {}: {}", id, source),
                None => write!(f, "Network error: {}", source),
            },
            UnorderedAggError::ChannelClosed { context } => {
                write!(f, "Control channel operation failed (channel closed or send error): {}", context)
            }
            UnorderedAggError::TimeoutOrRecvError { operation } => {
                write!(f, "Operation '{}' timed out or failed to receive response", operation)
            }
            UnorderedAggError::Internal { context, source } => match source {
                Some(e) => write!(f, "Internal error in {}: {}", context, e),
                None => write!(f, "Internal error in {}", context),
            },
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
        &self, data: &[u8], link_id: LinkId, session_id: u64, original_client: Option<std::net::SocketAddr>,
    ) -> Result<usize, std::io::Error> {
        // Create wrapped message
        let msg = UnorderedLinkMsg::Data { link_id, session_id, original_client, payload: data.to_vec() };

        // Serialize message
        let mut buf = Vec::new();
        msg.write(&mut buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

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
    /// Number of successful send operations
    pub successful_sends: Arc<AtomicU64>,
    /// Number of failed send operations
    pub failed_sends: Arc<AtomicU64>,
    /// Whether this link is considered healthy
    pub is_healthy: bool,
    /// 滑动窗口性能统计
    pub windowed_stats: Arc<RwLock<WindowedStats>>,
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
            successful_sends: Arc::new(AtomicU64::new(0)),
            failed_sends: Arc::new(AtomicU64::new(0)),
            is_healthy: true,
            windowed_stats: Arc::new(RwLock::new(WindowedStats::new(Duration::from_secs(60)))),
        }
    }

    /// Update statistics for sent data
    pub async fn record_sent(&self, bytes: usize) {
        self.bytes_sent.fetch_add(bytes as u64, Ordering::Relaxed);
        self.successful_sends.fetch_add(1, Ordering::Relaxed);

        // Update sliding window stats
        let mut windowed_stats = self.windowed_stats.write().await;
        windowed_stats.add_record(1, 0, bytes as u64);
    }

    /// Update statistics for received data
    pub fn record_received(&self, bytes: usize) {
        self.bytes_received.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Record a failed send operation
    pub async fn record_send_failure(&self) {
        self.failed_sends.fetch_add(1, Ordering::Relaxed);

        // Update sliding window stats
        let mut windowed_stats = self.windowed_stats.write().await;
        windowed_stats.add_record(0, 1, 0);
    }

    /// Calculate packet loss rate (0.0 to 1.0)
    pub fn packet_loss_rate(&self) -> f64 {
        let successful = self.successful_sends.load(Ordering::Relaxed);
        let failed = self.failed_sends.load(Ordering::Relaxed);
        let total = successful + failed;

        if total == 0 {
            0.0
        } else {
            failed as f64 / total as f64
        }
    }

    /// Get quality score (combines bandwidth and packet loss)
    /// Higher score means better quality
    pub fn quality_score(&self, bandwidth: f64) -> f64 {
        let loss_rate = self.packet_loss_rate();
        // Quality = bandwidth * (1 - loss_rate)^2
        // Square the loss penalty to heavily penalize lossy links
        bandwidth * (1.0 - loss_rate).powi(2)
    }

    /// Get windowed packet loss rate using sliding window statistics
    pub async fn windowed_packet_loss_rate(&self) -> f64 {
        let mut windowed_stats = self.windowed_stats.write().await;
        windowed_stats.windowed_packet_loss_rate()
    }

    /// Get windowed send speed using sliding window statistics
    pub async fn windowed_send_speed(&self) -> f64 {
        let mut windowed_stats = self.windowed_stats.write().await;
        windowed_stats.windowed_send_speed()
    }

    /// Calculate dynamic weight with exploration mechanism
    pub async fn dynamic_weight(&self, base_bandwidth: f64) -> f64 {
        let mut windowed_stats = self.windowed_stats.write().await;
        windowed_stats.dynamic_weight(base_bandwidth)
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
        link_id: LinkId,
    },
    /// Update link health status
    UpdateLinkHealth {
        /// Link identifier
        link_id: LinkId,
        /// Whether the link is healthy
        is_healthy: bool,
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
        Self { data, link_id, session_id: 0, original_client: None }
    }

    /// Create new received data with session information
    pub fn with_session(
        data: Vec<u8>, link_id: LinkId, session_id: u64, original_client: Option<std::net::SocketAddr>,
    ) -> Self {
        Self { data, link_id, session_id, original_client }
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
    _config: UnorderedCfg,
    /// Channel for sending received data to application
    rx_channel: mpsc::UnboundedSender<ReceivedData>,
    /// Channel for receiving control messages
    _control_tx: mpsc::UnboundedSender<UnorderedControlMsg>,
    /// Active links
    links: Arc<RwLock<HashMap<LinkId, UnorderedLinkState>>>,
    /// Load balancer state
    load_balancer: UnorderedLoadBalancer,
}

impl UnorderedAggTask {
    /// Create a new unordered aggregation task
    pub fn new(
        config: UnorderedCfg, rx_channel: mpsc::UnboundedSender<ReceivedData>,
        control_tx: mpsc::UnboundedSender<UnorderedControlMsg>,
    ) -> Self {
        Self {
            _config: config.clone(),
            rx_channel,
            _control_tx: control_tx,
            links: Arc::new(RwLock::new(HashMap::new())),
            load_balancer: UnorderedLoadBalancer::new(config.load_balance, config.node_role),
        }
    }

    /// Start the aggregation task
    pub async fn run(
        &self, mut control_rx: mpsc::UnboundedReceiver<UnorderedControlMsg>,
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
            return Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "No links available"));
        }

        let link_id = match self.load_balancer.select_link(&links).await {
            Some(id) => id,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "No links available for selection",
                ))
            }
        };

        if let Some(link_state) = links.get(&link_id) {
            match link_state.transport.send(&data).await {
                Ok(bytes_sent) => {
                    link_state.record_sent(bytes_sent).await;
                    Ok(())
                }
                Err(e) => {
                    error!("Failed to send data through link {:?}: {}", link_id, e);
                    Err(e)
                }
            }
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "Selected link not found"))
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
    pub async fn send_data_via_link(&self, data: Vec<u8>, link_id: LinkId) -> UnorderedAggResult<SendResult> {
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
                link_state_clone.record_sent(bytes_sent).await;
                info!("Successfully sent {} bytes via link {} in {:?}", bytes_sent, link_id, duration);
                Ok(SendResult { bytes_sent, link_id, duration, was_retry: false })
            }
            Err(e) => {
                let duration = start_time.elapsed();
                link_state_clone.record_send_failure().await;
                error!("Send via link {} failed after {:?}: {}", link_id, duration, e);
                Err(UnorderedAggError::NetworkError { link_id: Some(link_id), source: e })
            }
        }
    }

    /// Send data through the aggregation with session information
    pub async fn send_data_with_session(
        &self, data: Vec<u8>, session_id: u64, original_client: Option<std::net::SocketAddr>,
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
                    info!(
                        "Sending {} bytes with session {} via link {} ({})",
                        data_len, session_id, selected_link_id, link_state.remote_addr
                    );
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
                link_state_clone.record_sent(bytes_sent).await;
                info!(
                    "Successfully sent {} bytes with session {} via link {} in {:?}",
                    bytes_sent, session_id, selected_link_id, duration
                );
                Ok(SendResult {
                    bytes_sent,
                    link_id: selected_link_id,
                    duration,
                    was_retry: false, // Retries not implemented yet
                })
            }
            Err(e) => {
                let duration = start_time.elapsed();
                link_state_clone.record_send_failure().await;
                error!(
                    "Send with session {} via link {} failed after {:?}: {}",
                    session_id, selected_link_id, duration, e
                );
                Err(UnorderedAggError::NetworkError { link_id: Some(selected_link_id), source: e })
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
            error!("Failed to forward received data: {}", e);
        }
    }

    /// Handle incoming data from a link with session information
    async fn handle_incoming_data_with_session(
        &self, data: Vec<u8>, from_link: LinkId, _session_id: u64, _original_client: Option<std::net::SocketAddr>,
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
            error!("Failed to forward received data with session: {}", e);
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
            info!("Updated link {} health: {}", link_id, is_healthy);
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

            link_stats.insert(
                *link_id,
                UnorderedLinkStats {
                    remote_addr: link_state.remote_addr.clone(),
                    is_healthy: link_state.is_healthy,
                    bytes_sent,
                    bytes_received,
                    last_seen: link_state.last_seen,
                    avg_rtt: 0.0,          // Will be populated from load balancer metrics
                    bandwidth: 0.0,        // Will be populated from load balancer metrics
                    packet_loss_rate: 0.0, // Will be calculated from send stats
                    successful_sends: 0,   // To be tracked in future enhancement
                    failed_sends: 0,       // To be tracked in future enhancement
                },
            );
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
    /// Node role for directional bandwidth prioritization
    node_role: crate::unordered_cfg::NodeRole,
}

/// Performance metrics for each link
#[derive(Debug, Clone)]
struct LinkMetrics {
    /// Average round-trip time in milliseconds
    avg_rtt: f64,
    /// Sending bandwidth (bytes/sec) - important for servers
    send_bandwidth: f64,
    /// Receiving bandwidth (bytes/sec) - important for clients
    recv_bandwidth: f64,
    /// Last update timestamp
    _last_update: Instant,
}

impl UnorderedLoadBalancer {
    /// Create a new load balancer
    pub fn new(strategy: LoadBalanceStrategy, node_role: crate::unordered_cfg::NodeRole) -> Self {
        Self {
            strategy,
            round_robin_index: std::sync::atomic::AtomicUsize::new(0),
            link_metrics: Arc::new(RwLock::new(HashMap::new())),
            node_role,
        }
    }

    /// Update link performance metrics with directional bandwidth awareness
    pub async fn update_link_metrics(
        &self, link_id: LinkId, rtt: Duration, send_bytes_per_sec: f64, recv_bytes_per_sec: f64,
    ) {
        let mut metrics = self.link_metrics.write().await;
        metrics.insert(
            link_id,
            LinkMetrics {
                avg_rtt: rtt.as_millis() as f64,
                send_bandwidth: send_bytes_per_sec,
                recv_bandwidth: recv_bytes_per_sec,
                _last_update: Instant::now(),
            },
        );
    }

    /// Update link performance metrics (backward compatibility)
    pub async fn update_link_metrics_simple(&self, link_id: LinkId, rtt: Duration, bytes_per_sec: f64) {
        // Assume symmetric bandwidth for backward compatibility
        self.update_link_metrics(link_id, rtt, bytes_per_sec, bytes_per_sec).await;
    }

    /// Select a link for sending data
    pub async fn select_link(&self, links: &HashMap<LinkId, UnorderedLinkState>) -> Option<LinkId> {
        let healthy_links: Vec<_> = links.iter().filter(|(_, state)| state.is_healthy).collect();

        if healthy_links.is_empty() {
            // Fallback to any available link if no healthy ones
            return links.keys().next().copied();
        }

        match self.strategy {
            LoadBalanceStrategy::PacketRoundRobin => {
                // Packet-level round-robin without session affinity
                let index = self.round_robin_index.fetch_add(1, Ordering::Relaxed) % healthy_links.len();
                Some(*healthy_links[index].0)
            }
            LoadBalanceStrategy::WeightedByBandwidth => {
                // Use bandwidth-based weighted selection
                let metrics = self.link_metrics.read().await;
                let mut best_link = *healthy_links[0].0;
                let mut best_bandwidth = 0.0;

                for (link_id, _) in healthy_links {
                    if let Some(metric) = metrics.get(link_id) {
                        if metric.send_bandwidth > best_bandwidth {
                            best_bandwidth = metric.send_bandwidth;
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
            LoadBalanceStrategy::WeightedByPacketLoss => {
                // Weighted random selection using windowed packet loss rates and dynamic weights
                // Links with lower packet loss get higher probability, with exploration mechanism
                let mut weights = Vec::new();
                let mut total_weight = 0.0;

                // Calculate dynamic weights for each link
                for (link_id, link_state) in healthy_links.iter() {
                    // Use windowed packet loss rate for more responsive adjustment
                    let windowed_loss_rate = link_state.windowed_packet_loss_rate().await;

                    // Get base bandwidth from metrics (with fallback)
                    let metrics = self.link_metrics.read().await;
                    let base_bandwidth = metrics
                        .get(link_id)
                        .map(|m| match self.node_role {
                            crate::unordered_cfg::NodeRole::Client => m.recv_bandwidth,
                            crate::unordered_cfg::NodeRole::Server => m.send_bandwidth,
                            crate::unordered_cfg::NodeRole::Balanced => {
                                (m.send_bandwidth + m.recv_bandwidth) / 2.0
                            }
                        })
                        .unwrap_or(1.0);

                    // Calculate dynamic weight with exploration mechanism
                    let dynamic_weight = link_state.dynamic_weight(base_bandwidth).await;

                    weights.push((**link_id, dynamic_weight, windowed_loss_rate));
                    total_weight += dynamic_weight;

                    debug!(
                        "Link {} - windowed loss: {:.4}, base BW: {:.2}, dynamic weight: {:.4}",
                        link_id, windowed_loss_rate, base_bandwidth, dynamic_weight
                    );
                }

                if total_weight == 0.0 {
                    // Fallback to first link if all weights are zero
                    return Some(*healthy_links[0].0);
                }

                // Weighted random selection
                use rand::Rng;
                let mut rng = rand::rng();
                let mut random_value = rng.random::<f64>() * total_weight;

                for (link_id, weight, loss_rate) in &weights {
                    random_value -= weight;
                    if random_value <= 0.0 {
                        debug!(
                            "Selected link {} with dynamic weight {:.2}/{:.2} (windowed loss rate: {:.4})",
                            link_id, weight, total_weight, loss_rate
                        );
                        return Some(*link_id);
                    }
                }

                // Fallback to last link (shouldn't happen)
                Some(weights.last().unwrap().0)
            }
            LoadBalanceStrategy::DynamicAdaptive => {
                // Advanced dynamic selection using windowed statistics and exploration
                // This strategy automatically recovers from temporary link issues
                let metrics = self.link_metrics.read().await;
                let mut weights = Vec::new();
                let mut total_weight = 0.0;

                // Calculate dynamic weights for each link
                for (link_id, link_state) in healthy_links.iter() {
                    let send_bandwidth = metrics.get(link_id).map(|m| m.send_bandwidth).unwrap_or(1.0);

                    let recv_bandwidth = metrics.get(link_id).map(|m| m.recv_bandwidth).unwrap_or(1.0);

                    // Select primary bandwidth based on node role
                    let primary_bandwidth = match self.node_role {
                        crate::unordered_cfg::NodeRole::Client => recv_bandwidth,
                        crate::unordered_cfg::NodeRole::Server => send_bandwidth,
                        crate::unordered_cfg::NodeRole::Balanced => (send_bandwidth + recv_bandwidth) / 2.0,
                    };

                    // Calculate dynamic weight with exploration mechanism
                    let dynamic_weight = link_state.dynamic_weight(primary_bandwidth).await;

                    // Get debug info for logging
                    let mut windowed_stats = link_state.windowed_stats.write().await;
                    let (loss_rate, speed, entries_count) = windowed_stats.get_debug_info();
                    drop(windowed_stats); // Release lock

                    weights.push((**link_id, dynamic_weight, loss_rate, speed));
                    total_weight += dynamic_weight;

                    debug!("Link {} - primary BW: {:.2}, windowed loss: {:.4}, speed: {:.2}, weight: {:.4}, entries: {}",
                           link_id, primary_bandwidth, loss_rate, speed, dynamic_weight, entries_count);
                }

                if total_weight == 0.0 {
                    // Fallback to first link if all weights are zero
                    return Some(*healthy_links[0].0);
                }

                // Weighted random selection
                use rand::Rng;
                let mut rng = rand::rng();
                let mut random_value = rng.random::<f64>() * total_weight;

                for (link_id, weight, loss_rate, speed) in &weights {
                    random_value -= weight;
                    if random_value <= 0.0 {
                        debug!(
                            "Selected link {} with dynamic weight {:.4}/{:.4} (loss: {:.4}, speed: {:.2})",
                            link_id, weight, total_weight, loss_rate, speed
                        );
                        return Some(*link_id);
                    }
                }

                // Fallback to last link (shouldn't happen)
                Some(weights.last().unwrap().0)
            }
        }
    }

    /// Select a link for sending data - all strategies now use packet-level load balancing
    pub async fn select_link_with_session(
        &self, links: &HashMap<LinkId, UnorderedLinkState>, _session_id: u64,
    ) -> Option<LinkId> {
        // All strategies now ignore session affinity and use packet-level load balancing for maximum throughput
        self.select_link(links).await
    }
}

/// 滑动窗口性能统计
#[derive(Debug, Clone)]
pub struct WindowedStats {
    /// 时间窗口记录 (时间戳, 成功数, 失败数, 发送字节数)
    entries: VecDeque<(Instant, u64, u64, u64)>,
    /// 窗口大小
    window_duration: Duration,
    /// 最小探索权重（防止链路完全被忽略）
    min_exploration_weight: f64,
}

impl WindowedStats {
    fn new(window_duration: Duration) -> Self {
        Self {
            entries: VecDeque::new(),
            window_duration,
            min_exploration_weight: 0.1, // 至少保留10%的权重用于探索
        }
    }

    /// 添加新的统计记录
    fn add_record(&mut self, successful: u64, failed: u64, bytes_sent: u64) {
        let now = Instant::now();
        self.entries.push_back((now, successful, failed, bytes_sent));
        self.cleanup_old_entries(now);
    }

    /// 清理过期的记录
    fn cleanup_old_entries(&mut self, now: Instant) {
        while let Some(&(timestamp, _, _, _)) = self.entries.front() {
            if now.duration_since(timestamp) > self.window_duration {
                self.entries.pop_front();
            } else {
                break;
            }
        }
    }

    /// 获取窗口内的丢包率
    fn windowed_packet_loss_rate(&mut self) -> f64 {
        let now = Instant::now();
        self.cleanup_old_entries(now);

        let (total_successful, total_failed) =
            self.entries.iter().fold((0u64, 0u64), |(acc_s, acc_f), &(_, s, f, _)| (acc_s + s, acc_f + f));

        let total = total_successful + total_failed;
        if total == 0 {
            0.0
        } else {
            total_failed as f64 / total as f64
        }
    }

    /// 获取窗口内的平均发送速度
    fn windowed_send_speed(&mut self) -> f64 {
        let now = Instant::now();
        self.cleanup_old_entries(now);

        if self.entries.is_empty() {
            return 0.0;
        }

        let total_bytes: u64 = self.entries.iter().map(|(_, _, _, bytes)| bytes).sum();
        let oldest_time = self.entries.front().map(|(t, _, _, _)| *t).unwrap_or(now);
        let duration = now.duration_since(oldest_time).as_secs_f64();

        if duration > 0.0 {
            total_bytes as f64 / duration
        } else {
            0.0
        }
    }

    /// 计算动态权重（包含探索机制）
    fn dynamic_weight(&mut self, base_bandwidth: f64) -> f64 {
        let loss_rate = self.windowed_packet_loss_rate();
        let speed = self.windowed_send_speed();

        // 基础权重：考虑带宽和丢包率
        let quality_weight = (base_bandwidth + speed) * (1.0 - loss_rate).powi(2);

        // 添加探索权重：确保即使表现不好的链路也能获得一些流量来尝试恢复
        let exploration_weight = base_bandwidth * self.min_exploration_weight;

        // 总权重 = 质量权重 + 探索权重
        quality_weight + exploration_weight
    }

    /// 获取统计信息用于调试
    fn get_debug_info(&mut self) -> (f64, f64, usize) {
        (self.windowed_packet_loss_rate(), self.windowed_send_speed(), self.entries.len())
    }
}
