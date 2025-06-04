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
    sync::{Arc, atomic::{AtomicU64, Ordering}},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, RwLock};

use crate::{
    id::LinkId,
    unordered_cfg::{LoadBalanceStrategy, UnorderedCfg},
};

/// Trait for transport-agnostic link operations
#[async_trait::async_trait]
pub trait UnorderedLinkTransport: Send + Sync + std::fmt::Debug + 'static {
    /// Send data through this transport link
    async fn send(&self, data: &[u8]) -> Result<usize, std::io::Error>;
    
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
        respond_to: oneshot::Sender<usize>,
    },
    /// Send data through a specific link
    SendDataViaLink {
        /// Data to be sent
        data: Vec<u8>,
        /// Link identifier
        link_id: LinkId,
        /// Channel to send response
        respond_to: oneshot::Sender<usize>,
    },
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
    rx_channel: mpsc::UnboundedSender<(Vec<u8>, LinkId)>,
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
        rx_channel: mpsc::UnboundedSender<(Vec<u8>, LinkId)>,
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
                            let _ = respond_to.send(result.unwrap_or(0));
                        }
                        Some(UnorderedControlMsg::SendDataViaLink { data, link_id, respond_to }) => {
                            let result = self.send_data_via_link(data, link_id).await;
                            let _ = respond_to.send(result.unwrap_or(0));
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

        let link_id = self.load_balancer.select_link(&links).await;
        if let Some(link_state) = links.get(&link_id) {
            match link_state.transport.send(&data).await {
                Ok(bytes_sent) => {
                    link_state.record_sent(bytes_sent);
                    Ok(())
                }
                Err(e) => {
                    eprintln!("Failed to send data through link {}: {}", link_id, e);
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

    /// Send data through the aggregation
    pub async fn send_data(&self, data: Vec<u8>) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let links = self.links.read().await;
        
        if links.is_empty() {
            return Err("No links available".into());
        }

        let selected_link_id = self.load_balancer.select_link(&links).await;
        if let Some(link_state) = links.get(&selected_link_id) {
            if link_state.is_healthy {
                match link_state.transport.send(&data).await {
                    Ok(bytes_sent) => {
                        // Update statistics
                        return Ok(bytes_sent);
                    }
                    Err(e) => return Err(Box::new(e)),
                }
            }
        }
        
        Err("No healthy links available".into())
    }

    /// Send data through a specific link
    pub async fn send_data_via_link(
        &self, 
        data: Vec<u8>, 
        link_id: LinkId
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let links = self.links.read().await;
        
        if let Some(link_state) = links.get(&link_id) {
            match link_state.transport.send(&data).await {
                Ok(bytes_sent) => Ok(bytes_sent),
                Err(e) => Err(Box::new(e)),
            }
        } else {
            Err(format!("Link {} not found", link_id.0).into())
        }
    }

    /// Handle incoming data from a link
    async fn handle_incoming_data(&self, _data: Vec<u8>, _from_link: LinkId) {
        // In the unordered aggregation model, we simply forward all received data
        // to the application without any ordering or deduplication
        // TODO: Forward to rx_channel
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
    pub async fn select_link(&self, links: &HashMap<LinkId, UnorderedLinkState>) -> LinkId {
        let healthy_links: Vec<_> = links
            .iter()
            .filter(|(_, state)| state.is_healthy)
            .collect();

        if healthy_links.is_empty() {
            // Fallback to any available link
            return *links.keys().next().unwrap_or(&LinkId(0));
        }

        match self.strategy {
            LoadBalanceStrategy::RoundRobin => {
                let index = self.round_robin_index
                    .fetch_add(1, Ordering::Relaxed) % healthy_links.len();
                *healthy_links[index].0
            }
            LoadBalanceStrategy::Random => {
                use rand::Rng;
                let mut rng = rand::rng();
                let index = rng.random_range(0..healthy_links.len());
                *healthy_links[index].0
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
                best_link
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
                best_link
            }
        }
    }
}

/// Unordered aggregation handle for external API
pub struct UnorderedAggHandle {
    /// Control channel for sending commands
    control_tx: mpsc::UnboundedSender<UnorderedControlMsg>,
    /// Data channel for receiving incoming data  
    rx_channel: mpsc::UnboundedReceiver<(Vec<u8>, LinkId)>,
}

impl UnorderedAggHandle {
    /// Create a new handle
    pub fn new(
        control_tx: mpsc::UnboundedSender<UnorderedControlMsg>,
        rx_channel: mpsc::UnboundedReceiver<(Vec<u8>, LinkId)>,
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
    pub async fn send(&self, data: Vec<u8>) -> Result<(), mpsc::error::SendError<UnorderedControlMsg>> {
        let (tx, rx) = oneshot::channel();
        self.control_tx.send(UnorderedControlMsg::SendData { 
            data, 
            respond_to: tx 
        })?;
        // Wait for send completion
        let _ = rx.await;
        Ok(())
    }

    /// Send data through a specific link
    pub async fn send_via_link(
        &self, 
        data: Vec<u8>, 
        link_id: LinkId
    ) -> Result<(), mpsc::error::SendError<UnorderedControlMsg>> {
        let (tx, rx) = oneshot::channel();
        self.control_tx.send(UnorderedControlMsg::SendDataViaLink { 
            data, 
            link_id,
            respond_to: tx 
        })?;
        // Wait for send completion
        let _ = rx.await;
        Ok(())
    }

    /// Receive data from any link
    pub async fn recv(&mut self) -> Option<(Vec<u8>, LinkId)> {
        self.rx_channel.recv().await
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
        async fn send(&self, data: &[u8]) -> Result<usize, std::io::Error> {
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
