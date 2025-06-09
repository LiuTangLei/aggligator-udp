//! Unordered link aggregation configuration.
//!
//! This module provides configuration structures and utilities for
//! unordered aggregation mode, which prioritizes performance over
//! strict ordering guarantees. This is suitable for protocols like
//! UDP, QUIC, WebRTC, and other scenarios where the application
//! layer handles ordering and reliability.
//!
//! # Example Usage
//!
//! ```rust
//! use aggligator::unordered_cfg::{UnorderedCfg, LoadBalanceStrategy};
//! use aggligator::unordered_task::{UnorderedAggManager, HealthCheckConfig};
//! use tokio::sync::mpsc;
//! use std::time::Duration;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create configuration for high-performance UDP aggregation
//! let config = UnorderedCfg {
//!     load_balance: LoadBalanceStrategy::PacketRoundRobin,
//!     max_bandwidth_mbps: Some(1000), // 1 Gbps total
//!     enable_jitter_control: true,
//!     max_jitter_ms: 5,
//!     ..Default::default()
//! };
//!
//! // Set up channels for data forwarding
//! let (tx_rx, mut data_receiver) = mpsc::unbounded_channel();
//! let (tx_control, rx_control) = mpsc::unbounded_channel();
//!
//! // Create and start the aggregation manager
//! let manager = UnorderedAggManager::new(config, tx_rx, tx_control);
//!
//! // Start the aggregation task
//! tokio::spawn(async move {
//!     manager.get_task().run(rx_control).await
//! });
//!
//! // Add links (transport implementations would be provided by transport crates)
//! // manager.add_link(link_id1, udp_transport1).await?;
//! // manager.add_link(link_id2, quic_transport2).await?;
//!
//! // Send data through aggregated links
//! let data = b"Hello, aggregated world!".to_vec();
//! // manager.send_data(data).await?;
//!
//! // Receive aggregated data
//! while let Some(received) = data_receiver.recv().await {
//!     println!("Received {} bytes", received.data.len());
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Configuration Options
//!
//! The `UnorderedCfg` struct provides several options for tuning the
//! behavior of the unordered aggregation, including:
//!
//! - `send_queue` / `recv_queue`: Sizes of the send and receive queues.
//! - `load_balance`: Strategy for distributing packets across links.
//! - `heartbeat_interval` / `link_timeout`: Parameters for link health detection.
//! - `max_packet_size`: Maximum size of a single packet.
//! - `connect_queue`: Queue length for establishing new connections.
//! - `stats_intervals`: Intervals for calculating link speed statistics.
//!
//! ## Load Balancing Strategies
//!
//! The `LoadBalanceStrategy` enum defines several strategies for load
//! balancing across multiple links:
//!
//! - `PacketRoundRobin`: Round-robin at the packet level, allowing single UDP flows to use multiple links.
//! - `WeightedByBandwidth`: Distributes packets based on the bandwidth of each link.
//! - `FastestFirst`: Always uses the fastest (lowest latency) link available.
//! - `WeightedByPacketLoss`: Distributes packets based on packet loss rates, favoring reliable links.
//! - `DynamicAdaptive`: Advanced adaptive strategy combining bandwidth, packet loss, and exploration for optimal performance.
//!
//! ## Heartbeat and Timeout
//!
//! The `heartbeat_interval` and `link_timeout` fields in the configuration
//! control how the system detects and responds to link failures. The
//! heartbeat interval determines how often heartbeat packets are sent,
//! while the link timeout determines how long to wait for a heartbeat
//! response before considering the link as failed.
//!
//! ## Packet Size and Queues
//!
//! The `max_packet_size` field should be set according to the Maximum
//! Transmission Unit (MTU) of the network to avoid packet fragmentation.
//! The send and receive queues (`send_queue` and `recv_queue`) control
//! how many packets can be queued for sending and receiving, respectively.
//!
//! ## Statistics and Monitoring
//!
//! The `stats_intervals` field allows the configuration of multiple
//! intervals for monitoring link performance. These intervals are used
//! to calculate throughput statistics, which can inform load balancing
//! decisions.
//!
//! # Example
//!
//! Here is a more complete example demonstrating the usage of the
//! configuration and the aggregation manager:
//!
//! ```rust
//! use aggligator::unordered_cfg::{UnorderedCfg, LoadBalanceStrategy};
//! use aggligator::unordered_task::{UnorderedAggManager, HealthCheckConfig};
//! use tokio::sync::mpsc;
//! use std::time::Duration;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create configuration for high-performance UDP aggregation
//! let config = UnorderedCfg {
//!     load_balance: LoadBalanceStrategy::PacketRoundRobin,
//!     max_bandwidth_mbps: Some(1000), // 1 Gbps total
//!     enable_jitter_control: true,
//!     max_jitter_ms: 5,
//!     ..Default::default()
//! };
//!
//! // Set up channels for data forwarding
//! let (tx_rx, mut data_receiver) = mpsc::unbounded_channel();
//! let (tx_control, rx_control) = mpsc::unbounded_channel();
//!
//! // Create and start the aggregation manager
//! let manager = UnorderedAggManager::new(config, tx_rx, tx_control);
//!
//! // Start the aggregation task
//! tokio::spawn(async move {
//!     manager.get_task().run(rx_control).await
//! });
//!
//! // Add links (transport implementations would be provided by transport crates)
//! // manager.add_link(link_id1, udp_transport1).await?;
//! // manager.add_link(link_id2, quic_transport2).await?;
//!
//! // Send data through aggregated links
//! let data = b"Hello, aggregated world!".to_vec();
//! // manager.send_data(data).await?;
//!
//! // Receive aggregated data
//! while let Some(received) = data_receiver.recv().await {
//!     println!("Received {} bytes", received.data.len());
//! }
//! # Ok(())
//! # }
//! ```
//!
//! This example demonstrates creating a configuration for high-performance
//! UDP aggregation, setting up the necessary channels, creating and starting
//! the aggregation manager, and finally sending and receiving data through
//! the aggregated links.

use std::{num::NonZeroUsize, time::Duration};

/// Load balancing strategy for unordered aggregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "dump", derive(serde::Serialize, serde::Deserialize))]
pub enum LoadBalanceStrategy {
    /// Packet-level round-robin without session affinity.
    /// Each packet is distributed to links in round-robin fashion,
    /// allowing single UDP flows to use multiple links for bandwidth aggregation.
    PacketRoundRobin,
    /// Weighted distribution based on link bandwidth.
    /// Always selects the link with highest available bandwidth.
    WeightedByBandwidth,
    /// Always use the fastest (lowest latency) link first.
    /// Prioritizes latency over bandwidth utilization.
    FastestFirst,
    /// Weighted distribution based on packet loss rate.
    /// Links with lower packet loss get higher weight.
    /// This strategy monitors packet loss rates and automatically
    /// reduces traffic to lossy links while favoring reliable ones.
    WeightedByPacketLoss,
    /// Dynamic adaptive distribution with automatic weight recovery.
    /// Uses sliding window statistics and includes exploration mechanism
    /// to prevent links from being permanently marginalized due to temporary issues.
    /// Features:
    /// - Sliding window packet loss and bandwidth tracking
    /// - Minimum exploration weight (prevents total exclusion)
    /// - Dynamic weight adjustment based on recent performance
    /// - Automatic recovery when link conditions improve
    /// - Weighted random selection for optimal load balancing
    DynamicAdaptive,
}

impl Default for LoadBalanceStrategy {
    fn default() -> Self {
        // Use PacketRoundRobin by default for true bandwidth aggregation
        Self::PacketRoundRobin
    }
}

/// Role of the node in aggregation (affects bandwidth priority)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "dump", derive(serde::Serialize, serde::Deserialize))]
pub enum NodeRole {
    /// Client role - prioritizes receiving bandwidth (download speed)
    Client,
    /// Server role - prioritizes sending bandwidth (upload speed)
    Server,
    /// Balanced role - considers both directions equally
    Balanced,
}

/// Configuration for unordered link aggregation.
///
/// This is a simplified configuration focused on high-performance,
/// order-agnostic packet aggregation. Unlike ordered aggregation,
/// unordered aggregation does not guarantee packet ordering or reliability -
/// these concerns are handled by the application layer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "dump", derive(serde::Serialize, serde::Deserialize))]
pub struct UnorderedCfg {
    /// Length of queue for sending data packets.
    ///
    /// This controls how many packets can be queued for sending
    /// when links are temporarily unavailable.
    #[cfg_attr(
        feature = "dump",
        serde(
            serialize_with = "serde_helpers::serialize_nonzero_usize",
            deserialize_with = "serde_helpers::deserialize_nonzero_usize"
        )
    )]
    pub send_queue: NonZeroUsize,

    /// Length of queue for received data packets.
    ///
    /// This controls how many packets can be buffered on the
    /// receive side before backpressure is applied.
    #[cfg_attr(
        feature = "dump",
        serde(
            serialize_with = "serde_helpers::serialize_nonzero_usize",
            deserialize_with = "serde_helpers::deserialize_nonzero_usize"
        )
    )]
    pub recv_queue: NonZeroUsize,

    /// Load balancing strategy for distributing packets across links.
    pub load_balance: LoadBalanceStrategy,

    /// Interval for sending heartbeat packets to detect link health.
    ///
    /// Shorter intervals provide faster failure detection but increase overhead.
    #[cfg_attr(
        feature = "dump",
        serde(
            serialize_with = "serde_helpers::serialize_duration",
            deserialize_with = "serde_helpers::deserialize_duration"
        )
    )]
    pub heartbeat_interval: Duration,

    /// Timeout for considering a link as failed.
    ///
    /// If no heartbeat response is received within this timeout,
    /// the link is considered failed and removed from rotation.
    #[cfg_attr(
        feature = "dump",
        serde(
            serialize_with = "serde_helpers::serialize_duration",
            deserialize_with = "serde_helpers::deserialize_duration"
        )
    )]
    pub link_timeout: Duration,

    /// Maximum size of a single packet.
    ///
    /// Should be set considering MTU to avoid fragmentation.
    /// Typical values: 1472 (Ethernet), 1232 (safe), 8192 (jumbo frames).
    pub max_packet_size: usize,

    /// Queue length for establishing new connections.
    #[cfg_attr(
        feature = "dump",
        serde(
            serialize_with = "serde_helpers::serialize_nonzero_usize",
            deserialize_with = "serde_helpers::deserialize_nonzero_usize"
        )
    )]
    pub connect_queue: NonZeroUsize,

    /// Link speed statistics interval durations.
    ///
    /// These intervals are used to calculate throughput statistics
    /// for load balancing decisions.
    #[cfg_attr(
        feature = "dump",
        serde(
            serialize_with = "serde_helpers::serialize_duration_vec",
            deserialize_with = "serde_helpers::deserialize_duration_vec"
        )
    )]
    pub stats_intervals: Vec<Duration>,

    /// Node role for directional bandwidth prioritization.
    ///
    /// This affects how DynamicAdaptive strategy weighs send vs receive bandwidth:
    /// - Client: prioritizes receive bandwidth (download speed)
    /// - Server: prioritizes send bandwidth (upload speed)
    /// - Balanced: considers both directions equally
    pub node_role: NodeRole,
}

impl Default for UnorderedCfg {
    /// Default configuration optimized for typical unordered aggregation scenarios.
    ///
    /// This configuration prioritizes low latency and high throughput
    /// with reasonable resource usage.
    fn default() -> Self {
        Self {
            // Smaller queues than TCP version for lower latency
            send_queue: NonZeroUsize::new(256).unwrap(),
            recv_queue: NonZeroUsize::new(256).unwrap(),

            // Start with round-robin for simplicity
            load_balance: LoadBalanceStrategy::default(),

            // Faster heartbeat for quicker failure detection
            heartbeat_interval: Duration::from_millis(100),
            link_timeout: Duration::from_millis(500),

            // Conservative packet size to avoid fragmentation
            max_packet_size: 1472,

            // Small connect queue for faster setup
            connect_queue: NonZeroUsize::new(16).unwrap(),

            // Statistics intervals for performance monitoring
            stats_intervals: vec![
                Duration::from_millis(100), // 100ms for real-time monitoring
                Duration::from_secs(1),     // 1s for short-term trends
                Duration::from_secs(10),    // 10s for medium-term trends
            ],

            // Default to balanced role
            node_role: NodeRole::Balanced,
        }
    }
}

impl UnorderedCfg {
    /// Creates a configuration optimized for low-latency scenarios.
    ///
    /// This reduces queue sizes and heartbeat intervals for minimal latency
    /// at the cost of potentially higher overhead.
    pub fn low_latency() -> Self {
        Self {
            send_queue: NonZeroUsize::new(64).unwrap(),
            recv_queue: NonZeroUsize::new(64).unwrap(),
            load_balance: LoadBalanceStrategy::FastestFirst,
            heartbeat_interval: Duration::from_millis(50),
            link_timeout: Duration::from_millis(200),
            max_packet_size: 1200, // Conservative for lowest latency
            ..Self::default()
        }
    }

    /// Creates a configuration optimized for high-throughput scenarios.
    ///
    /// This increases queue sizes and uses bandwidth-weighted load balancing
    /// for maximum throughput.
    pub fn high_throughput() -> Self {
        Self {
            send_queue: NonZeroUsize::new(1024).unwrap(),
            recv_queue: NonZeroUsize::new(1024).unwrap(),
            load_balance: LoadBalanceStrategy::WeightedByBandwidth,
            heartbeat_interval: Duration::from_millis(200),
            link_timeout: Duration::from_secs(1),
            max_packet_size: 8192, // Use jumbo frames if available
            ..Self::default()
        }
    }

    /// Creates a configuration optimized for unreliable networks.
    ///
    /// This uses more aggressive heartbeats and shorter timeouts
    /// for better resilience in unstable network conditions.
    pub fn unreliable_network() -> Self {
        Self {
            load_balance: LoadBalanceStrategy::DynamicAdaptive, // Smart adaptation to unreliable conditions
            heartbeat_interval: Duration::from_millis(50),
            link_timeout: Duration::from_millis(150),
            max_packet_size: 1200, // Conservative to reduce loss
            ..Self::default()
        }
    }

    /// Validates the configuration and returns any issues found.
    pub fn validate(&self) -> Result<(), String> {
        if self.heartbeat_interval >= self.link_timeout {
            return Err("heartbeat_interval must be less than link_timeout".to_string());
        }

        if self.max_packet_size < 64 {
            return Err("max_packet_size must be at least 64 bytes".to_string());
        }

        if self.max_packet_size > 65507 {
            return Err("max_packet_size cannot exceed 65507 bytes (standard limit)".to_string());
        }

        if self.stats_intervals.is_empty() {
            return Err("at least one stats interval must be specified".to_string());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_is_valid() {
        let cfg = UnorderedCfg::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_preset_configs_are_valid() {
        assert!(UnorderedCfg::low_latency().validate().is_ok());
        assert!(UnorderedCfg::high_throughput().validate().is_ok());
        assert!(UnorderedCfg::unreliable_network().validate().is_ok());
    }

    #[test]
    fn test_invalid_config_detection() {
        let mut cfg = UnorderedCfg::default();

        // Test heartbeat >= timeout
        cfg.heartbeat_interval = Duration::from_secs(1);
        cfg.link_timeout = Duration::from_millis(500);
        assert!(cfg.validate().is_err());

        // Test packet size too small
        cfg = UnorderedCfg::default();
        cfg.max_packet_size = 32;
        assert!(cfg.validate().is_err());

        // Test packet size too large
        cfg = UnorderedCfg::default();
        cfg.max_packet_size = 70000;
        assert!(cfg.validate().is_err());
    }
}

#[cfg(feature = "dump")]
mod serde_helpers {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::num::NonZeroUsize;
    use std::time::Duration;

    pub fn serialize_duration<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        duration.as_millis().serialize(serializer)
    }

    pub fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let millis = u64::deserialize(deserializer)?;
        Ok(Duration::from_millis(millis))
    }

    pub fn serialize_nonzero_usize<S>(value: &NonZeroUsize, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value.get().serialize(serializer)
    }

    pub fn deserialize_nonzero_usize<'de, D>(deserializer: D) -> Result<NonZeroUsize, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = usize::deserialize(deserializer)?;
        NonZeroUsize::new(value).ok_or_else(|| serde::de::Error::custom("value must be non-zero"))
    }

    pub fn serialize_duration_vec<S>(durations: &Vec<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let millis: Vec<u64> = durations.iter().map(|d| d.as_millis() as u64).collect();
        millis.serialize(serializer)
    }

    pub fn deserialize_duration_vec<'de, D>(deserializer: D) -> Result<Vec<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let millis = Vec::<u64>::deserialize(deserializer)?;
        Ok(millis.into_iter().map(Duration::from_millis).collect())
    }
}
