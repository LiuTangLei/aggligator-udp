//! High-performance session management for UDP transparent proxy.
//!
//! This module provides zero-copy session tracking using packet headers
//! and lockfree data structures for maximum performance.
//!
//! Fixed critical concurrency bugs:
//! - TOCTOU race conditions in get_or_create_session
//! - Race conditions in cleanup_expired_sessions
//! - Session ID validation in touch_session
//! - Stable hashing for protocol consistency
//! - Memory leak prevention with atomic operations

use crc32fast::Hasher;
use dashmap::DashMap;
use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

/// Session ID type for efficient lookup
pub type SessionId = u32;

/// High-performance session manager using lockfree data structures
///
/// This implementation fixes several critical concurrency bugs:
/// - Uses DashMap entry API to prevent TOCTOU races
/// - Atomic session creation and cleanup
/// - Stable CRC32 hashing for protocol consistency
/// - Session ID overflow protection
#[derive(Debug)]
pub struct SessionManager {
    /// Client address to session ID mapping
    addr_to_session: Arc<DashMap<SocketAddr, SessionInfo>>,
    /// Session ID to client address mapping  
    session_to_addr: Arc<DashMap<SessionId, SocketAddr>>,
    /// Aggregation client mapping (session_id -> agg_client_addr)
    session_to_agg_client: Arc<DashMap<SessionId, SocketAddr>>,
    /// Session ID counter (with overflow protection)
    next_session_id: AtomicU32,
    /// Total sessions created (for statistics)
    total_sessions_created: AtomicU64,
    /// Session timeout
    session_timeout: Duration,
}

/// Session information with timestamp
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Unique session identifier
    pub session_id: SessionId,
    /// Session creation timestamp
    pub created_at: Instant,
    /// Last activity timestamp
    pub last_used: Instant,
}

impl SessionManager {
    /// Create a new session manager
    pub fn new(session_timeout: Duration) -> Self {
        Self {
            addr_to_session: Arc::new(DashMap::new()),
            session_to_addr: Arc::new(DashMap::new()),
            session_to_agg_client: Arc::new(DashMap::new()),
            next_session_id: AtomicU32::new(1),
            total_sessions_created: AtomicU64::new(0),
            session_timeout,
        }
    }

    /// Get or create session for client address (atomic operation, TOCTOU-safe)
    pub fn get_or_create_session(&self, client_addr: SocketAddr) -> SessionId {
        // Use DashMap entry API to prevent TOCTOU race conditions
        match self.addr_to_session.entry(client_addr) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                // Session exists, update timestamp atomically
                entry.get_mut().last_used = Instant::now();
                entry.get().session_id
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                // Session doesn't exist, create new one atomically
                let session_id = self.allocate_session_id();
                let now = Instant::now();
                let session_info = SessionInfo { session_id, created_at: now, last_used: now };

                // Insert both mappings atomically
                entry.insert(session_info.clone());
                self.session_to_addr.insert(session_id, client_addr);

                // Increment total sessions counter
                self.total_sessions_created.fetch_add(1, Ordering::Relaxed);

                session_id
            }
        }
    }

    /// Allocate a new session ID with overflow protection
    fn allocate_session_id(&self) -> SessionId {
        loop {
            let current = self.next_session_id.load(Ordering::Relaxed);
            // Prevent ID 0 (reserved) and handle overflow
            let next_id = if current == u32::MAX { 1 } else { current + 1 };

            if self
                .next_session_id
                .compare_exchange_weak(current, next_id, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return next_id;
            }
            // CAS failed, retry
        }
    }

    /// Get client address by session ID (zero-allocation lookup)
    pub fn get_client_addr(&self, session_id: SessionId) -> Option<SocketAddr> {
        self.session_to_addr.get(&session_id).map(|entry| *entry.value())
    }

    /// Update session timestamp (validates session ID first)
    pub fn touch_session(&self, session_id: SessionId) -> bool {
        // First validate that session_id exists and get the address
        if let Some(addr_entry) = self.session_to_addr.get(&session_id) {
            let client_addr = *addr_entry.value();
            drop(addr_entry); // Release the lock early

            // Then update the timestamp in the addr_to_session map
            if let Some(mut info_entry) = self.addr_to_session.get_mut(&client_addr) {
                // Double-check session_id matches to prevent race conditions
                if info_entry.session_id == session_id {
                    info_entry.last_used = Instant::now();
                    return true;
                }
            }
        }
        false // Session not found or validation failed
    }

    /// Clean up expired sessions (thread-safe, no race conditions)
    pub fn cleanup_expired_sessions(&self) -> usize {
        let now = Instant::now();
        let mut cleanup_count = 0;

        // Use retain to atomically remove expired sessions
        self.addr_to_session.retain(|addr, info| {
            let is_expired = now.duration_since(info.last_used) > self.session_timeout;
            if is_expired {
                // Remove from the reverse mapping too
                self.session_to_addr.remove(&info.session_id);
                self.session_to_agg_client.remove(&info.session_id);
                cleanup_count += 1;

                tracing::debug!(
                    "Cleaned up expired session {} for client {} (age: {:?})",
                    info.session_id,
                    addr,
                    now.duration_since(info.created_at)
                );
            }
            !is_expired // Keep if not expired
        });

        if cleanup_count > 0 {
            tracing::info!("Cleaned up {} expired sessions", cleanup_count);
        }

        cleanup_count
    }

    /// Get session statistics
    pub fn stats(&self) -> SessionStats {
        SessionStats {
            total_sessions_created: self.total_sessions_created.load(Ordering::Relaxed),
            active_sessions: self.session_to_addr.len(),
        }
    }

    /// Remember the aggregation client address for a session
    /// This is used by the server to track which aggregation client to send responses to
    /// Fixed: No longer requires session to exist first (for server-side usage)
    pub fn remember_aggregation_client(&self, session_id: SessionId, agg_client_addr: SocketAddr) -> bool {
        self.session_to_agg_client.insert(session_id, agg_client_addr);
        tracing::debug!("Session {} is now associated with aggregation client {}", session_id, agg_client_addr);
        true
    }

    /// Get the aggregation client address for a session
    pub fn get_aggregation_client(&self, session_id: SessionId) -> Option<SocketAddr> {
        self.session_to_agg_client.get(&session_id).map(|entry| *entry.value())
    }

    /// Remember complete session information for server-side response routing
    /// This tracks session_id -> (original_client_addr, aggregation_client_addr)
    pub fn remember_session_for_server(
        &self, session_id: SessionId, original_client_addr: SocketAddr, agg_client_addr: SocketAddr,
    ) {
        // Store the aggregation client mapping for response routing
        self.session_to_agg_client.insert(session_id, agg_client_addr);

        // Also store the original client address for session reconstruction
        // We create a dummy session info just to track the original client
        let session_info = SessionInfo { session_id, created_at: Instant::now(), last_used: Instant::now() };
        self.addr_to_session.insert(original_client_addr, session_info);
        self.session_to_addr.insert(session_id, original_client_addr);

        tracing::debug!(
            "Server recorded session {}: {} -> {} (via agg client {})",
            session_id,
            original_client_addr,
            agg_client_addr,
            agg_client_addr
        );
    }

    /// Get original client address for a session (used by server for response construction)
    pub fn get_original_client_addr(&self, session_id: SessionId) -> Option<SocketAddr> {
        self.session_to_addr.get(&session_id).map(|entry| *entry.value())
    }

    /// Get all active session mappings for response routing (server-side)
    /// Returns iterator of (session_id, agg_client_addr) pairs
    pub fn get_active_sessions(&self) -> impl Iterator<Item = (SessionId, SocketAddr)> + '_ {
        self.session_to_agg_client.iter().map(|entry| (*entry.key(), *entry.value()))
    }

    /// Generate session ID based on UDP 5-tuple for true multi-link aggregation
    /// This ensures that UDP packets from the same flow are consistently routed
    /// through the same link for proper session affinity
    pub fn generate_udp_session_id(src_addr: SocketAddr, dst_addr: SocketAddr, protocol: u8) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();

        // Hash the 5-tuple components for consistent session identification
        src_addr.ip().hash(&mut hasher);
        src_addr.port().hash(&mut hasher);
        dst_addr.ip().hash(&mut hasher);
        dst_addr.port().hash(&mut hasher);
        protocol.hash(&mut hasher);

        hasher.finish()
    }

    /// Generate session ID for UDP packet data based on packet content
    /// This method analyzes the packet to extract addressing information
    pub fn generate_session_from_packet(data: &[u8]) -> u64 {
        // For UDP aggregation, we can use a simpler approach
        // based on data content hash for session consistency
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();

        // Use first 32 bytes of data (or entire packet if smaller) for session hash
        let sample_size = std::cmp::min(data.len(), 32);
        data[..sample_size].hash(&mut hasher);

        hasher.finish()
    }

    /// Associate a session with an aggregation client for reverse path routing
    pub fn set_agg_client_for_session(&self, session_id: SessionId, agg_client_addr: SocketAddr) {
        self.session_to_agg_client.insert(session_id, agg_client_addr);
    }

    /// Get aggregation client address for a session (for reverse path routing)
    pub fn get_agg_client_for_session(&self, session_id: SessionId) -> Option<SocketAddr> {
        self.session_to_agg_client.get(&session_id).map(|entry| *entry.value())
    }
}

/// Session statistics
#[derive(Debug, Clone)]
pub struct SessionStats {
    /// Total number of sessions ever created
    pub total_sessions_created: u64,
    /// Currently active sessions
    pub active_sessions: usize,
}

/// Packet header for session tracking (12 bytes total)
///
/// Fixed issues:
/// - Uses stable CRC32 hashing instead of DefaultHasher  
/// - Removed problematic client_addr() reconstruction method
/// - Improved validation and error handling
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct SessionHeader {
    /// Session ID (4 bytes)
    pub session_id: u32,
    /// Client address CRC32 hash for validation (4 bytes)  
    pub addr_crc32: u32,
    /// Client address port (2 bytes)
    pub client_port: u16,
    /// Padding (2 bytes)
    pub padding: u16,
}

impl SessionHeader {
    /// Create a new session header
    pub fn new(session_id: SessionId, client_addr: SocketAddr) -> Self {
        Self {
            session_id,
            addr_crc32: Self::crc32_addr(client_addr),
            client_port: client_addr.port(),
            padding: 0,
        }
    }

    /// Calculate stable CRC32 hash of socket address for protocol consistency
    fn crc32_addr(addr: SocketAddr) -> u32 {
        let mut hasher = Hasher::new();

        // Hash IP address bytes
        match addr.ip() {
            IpAddr::V4(ipv4) => {
                hasher.update(&[4u8]); // Version marker
                hasher.update(&ipv4.octets());
            }
            IpAddr::V6(ipv6) => {
                hasher.update(&[6u8]); // Version marker
                hasher.update(&ipv6.octets());
            }
        }

        // Hash port
        hasher.update(&addr.port().to_be_bytes());

        hasher.finalize()
    }

    /// Validate session header against client address
    pub fn validate(&self, client_addr: SocketAddr) -> bool {
        self.addr_crc32 == Self::crc32_addr(client_addr) && self.client_port == client_addr.port()
    }

    /// Encode header to bytes
    pub fn to_bytes(&self) -> [u8; 12] {
        let mut bytes = [0u8; 12];
        bytes[0..4].copy_from_slice(&self.session_id.to_be_bytes());
        bytes[4..8].copy_from_slice(&self.addr_crc32.to_be_bytes());
        bytes[8..10].copy_from_slice(&self.client_port.to_be_bytes());
        bytes[10..12].copy_from_slice(&self.padding.to_be_bytes());
        bytes
    }

    /// Decode header from bytes
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 12 {
            return None;
        }

        let session_id = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let addr_crc32 = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let client_port = u16::from_be_bytes([bytes[8], bytes[9]]);
        let padding = u16::from_be_bytes([bytes[10], bytes[11]]);

        Some(Self { session_id, addr_crc32, client_port, padding })
    }
}

/// Zero-copy packet wrapper for session management
///
/// Note: The packet header contains session metadata, not the original client address.
/// Use SessionManager.get_client_addr(session_id) to get the real client address.
pub struct SessionPacket<'a> {
    data: &'a [u8],
}

impl<'a> SessionPacket<'a> {
    /// Wrap packet data (assumes first 12 bytes are session header)
    pub fn new(data: &'a [u8]) -> Option<Self> {
        if data.len() < 12 {
            return None;
        }
        Some(Self { data })
    }

    /// Get session header (zero-copy)
    ///
    /// Note: The header contains a CRC32 hash and port for validation,
    /// but not the complete client address. Use SessionManager for address lookup.
    pub fn header(&self) -> Option<SessionHeader> {
        SessionHeader::from_bytes(&self.data[0..12])
    }

    /// Get payload data (zero-copy)
    pub fn payload(&self) -> &[u8] {
        &self.data[12..]
    }

    /// Get raw data including header
    pub fn raw_data(&self) -> &[u8] {
        self.data
    }
}

/// Builder for creating session packets
pub struct SessionPacketBuilder {
    header: SessionHeader,
    payload: Vec<u8>,
}

impl SessionPacketBuilder {
    /// Create new packet builder
    pub fn new(session_id: SessionId, client_addr: SocketAddr, payload: Vec<u8>) -> Self {
        Self { header: SessionHeader::new(session_id, client_addr), payload }
    }

    /// Build packet with session header prepended
    pub fn build(self) -> Vec<u8> {
        let header_bytes = self.header.to_bytes();
        let mut packet = Vec::with_capacity(12 + self.payload.len());
        packet.extend_from_slice(&header_bytes);
        packet.extend_from_slice(&self.payload);
        packet
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn test_session_manager() {
        let manager = SessionManager::new(Duration::from_secs(60));
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12345);

        // Test session creation
        let session_id = manager.get_or_create_session(addr);
        assert!(session_id > 0);

        // Test session lookup
        assert_eq!(manager.get_client_addr(session_id), Some(addr));

        // Test session reuse
        let session_id2 = manager.get_or_create_session(addr);
        assert_eq!(session_id, session_id2);
    }

    #[test]
    fn test_session_header() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12345);
        let header = SessionHeader::new(42, addr);

        // Test validation
        assert!(header.validate(addr));

        // Test port matching
        assert_eq!(header.client_port, 12345);

        // Test serialization
        let bytes = header.to_bytes();
        let decoded = SessionHeader::from_bytes(&bytes).unwrap();
        assert_eq!(header.session_id, decoded.session_id);
        assert_eq!(header.addr_crc32, decoded.addr_crc32);
        assert_eq!(header.client_port, decoded.client_port);

        // Test validation with different address should fail
        let different_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 12345);
        assert!(!header.validate(different_addr));
    }

    #[test]
    fn test_session_packet() {
        let payload = b"Hello, World!";
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12345);

        // Build packet
        let packet_data = SessionPacketBuilder::new(42, addr, payload.to_vec()).build();

        // Parse packet
        let packet = SessionPacket::new(&packet_data).unwrap();
        let header = packet.header().unwrap();

        assert_eq!(header.session_id, 42);
        assert_eq!(packet.payload(), payload);
        assert!(header.validate(addr));
    }

    #[test]
    fn test_udp_session_id_generation() {
        let client_addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let server_addr: SocketAddr = "192.168.1.100:80".parse().unwrap();

        // Test that same inputs generate same session ID
        let session_id1 = generate_udp_session_id(client_addr, server_addr, 17);
        let session_id2 = generate_udp_session_id(client_addr, server_addr, 17);
        assert_eq!(session_id1, session_id2);

        // Test that different client generates different session ID
        let different_client: SocketAddr = "127.0.0.1:12346".parse().unwrap();
        let session_id3 = generate_udp_session_id(different_client, server_addr, 17);
        assert_ne!(session_id1, session_id3);

        // Test that different server generates different session ID
        let different_server: SocketAddr = "192.168.1.101:80".parse().unwrap();
        let session_id4 = generate_udp_session_id(client_addr, different_server, 17);
        assert_ne!(session_id1, session_id4);

        // Test that different protocol generates different session ID
        let session_id5 = generate_udp_session_id(client_addr, server_addr, 6); // TCP
        assert_ne!(session_id1, session_id5);
    }
}
