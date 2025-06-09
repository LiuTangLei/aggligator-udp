//! Unordered aggregation message types.
//!
//! This module defines a simplified message protocol for unordered aggregation
//! that removes the complexity of ordered delivery and acknowledgments
//! found in the ordered aggregation protocol. This can be used with UDP,
//! QUIC, WebRTC, and other unordered transport protocols.

use byteorder::{ReadBytesExt, WriteBytesExt, BE};
use std::io;

use crate::{
    id::LinkId,
    unordered_cfg::{NodeRole, UnorderedCfg},
};

/// Unordered link message types.
///
/// These messages are much simpler than ordered aggregation messages
/// since they don't need to handle ordering, acknowledgments, or reliability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnorderedLinkMsg {
    /// Initial handshake from server to client.
    Welcome {
        /// Configuration from server.
        cfg: UnorderedCfg,
        /// Server-assigned link identifier.
        link_id: LinkId,
    },

    /// Client requests to join aggregation group.
    Connect {
        /// Target aggregation group identifier.
        group_id: u64,
        /// User-specified connection data.
        user_data: Vec<u8>,
        /// Client configuration.
        cfg: UnorderedCfg,
    },

    /// Server accepts the connection.
    Accepted {
        /// Assigned link identifier for this connection.
        link_id: LinkId,
        /// Final negotiated configuration.
        cfg: UnorderedCfg,
    },

    /// Server refuses the connection.
    Refused {
        /// Reason for refusal.
        reason: RefusedReason,
    },

    /// Heartbeat request.
    Ping {
        /// Timestamp when ping was sent (for RTT calculation).
        timestamp: u64,
    },

    /// Heartbeat response.
    Pong {
        /// Original timestamp from ping.
        timestamp: u64,
    },

    /// Data packet.
    ///
    /// Unlike TCP aggregation, this doesn't need sequence numbers
    /// since ordering is handled by the application.
    /// But for transparent proxy, we need session tracking.
    Data {
        /// Link that sent this data.
        link_id: LinkId,
        /// Session ID for transparent proxy (optional, 0 means no session)
        session_id: u64,
        /// Original client address for transparent proxy (optional)
        original_client: Option<std::net::SocketAddr>,
        /// Actual data payload being transmitted
        payload: Vec<u8>,
    },

    /// Link status change notification.
    LinkStatus {
        /// Link identifier.
        link_id: LinkId,
        /// New status.
        status: LinkStatus,
    },

    /// Graceful disconnection.
    Goodbye {
        /// Reason for disconnection.
        reason: String,
    },
}

/// Reasons why a connection might be refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusedReason {
    /// Aggregation group not found.
    GroupNotFound,
    /// Aggregation group is full.
    GroupFull,
    /// Incompatible configuration.
    IncompatibleConfig,
    /// Server is shutting down.
    ServerShutdown,
    /// Authentication failed.
    AuthFailed,
    /// Rate limited.
    RateLimited,
}

/// Link status for unordered aggregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStatus {
    /// Link is active and ready for traffic.
    Active,
    /// Link is temporarily blocked (e.g., for testing).
    Blocked,
    /// Link is experiencing high latency.
    Slow,
    /// Link is disconnecting.
    Disconnecting,
}

impl UnorderedLinkMsg {
    /// Protocol version for unordered aggregation.
    pub const PROTOCOL_VERSION: u8 = 2;

    /// Magic identifier for unordered aggregation.
    const MAGIC: &'static [u8; 5] = b"UAGG\0";

    // Message type constants
    const MSG_WELCOME: u8 = 1;
    const MSG_CONNECT: u8 = 2;
    const MSG_ACCEPTED: u8 = 3;
    const MSG_REFUSED: u8 = 4;
    const MSG_PING: u8 = 5;
    const MSG_PONG: u8 = 6;
    const MSG_DATA: u8 = 7;
    const MSG_LINK_STATUS: u8 = 8;
    const MSG_GOODBYE: u8 = 9;

    /// Serializes the message to bytes.
    pub fn write(&self, mut writer: impl io::Write) -> Result<(), io::Error> {
        match self {
            UnorderedLinkMsg::Welcome { cfg, link_id } => {
                writer.write_u8(Self::MSG_WELCOME)?;
                writer.write_all(Self::MAGIC)?;
                writer.write_u8(Self::PROTOCOL_VERSION)?;
                writer.write_u128::<BE>(link_id.0)?;
                cfg.write(&mut writer)?;
            }

            UnorderedLinkMsg::Connect { group_id, user_data, cfg } => {
                writer.write_u8(Self::MSG_CONNECT)?;
                writer.write_all(Self::MAGIC)?;
                writer.write_u8(Self::PROTOCOL_VERSION)?;
                writer.write_u64::<BE>(*group_id)?;
                writer.write_u16::<BE>(
                    user_data
                        .len()
                        .try_into()
                        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "user data too long"))?,
                )?;
                writer.write_all(user_data)?;
                cfg.write(&mut writer)?;
            }

            UnorderedLinkMsg::Accepted { link_id, cfg } => {
                writer.write_u8(Self::MSG_ACCEPTED)?;
                writer.write_u128::<BE>(link_id.0)?;
                cfg.write(&mut writer)?;
            }

            UnorderedLinkMsg::Refused { reason } => {
                writer.write_u8(Self::MSG_REFUSED)?;
                reason.write(&mut writer)?;
            }

            UnorderedLinkMsg::Ping { timestamp } => {
                writer.write_u8(Self::MSG_PING)?;
                writer.write_u64::<BE>(*timestamp)?;
            }

            UnorderedLinkMsg::Pong { timestamp } => {
                writer.write_u8(Self::MSG_PONG)?;
                writer.write_u64::<BE>(*timestamp)?;
            }

            UnorderedLinkMsg::Data { link_id, session_id, original_client, payload } => {
                writer.write_u8(Self::MSG_DATA)?;
                writer.write_u128::<BE>(link_id.0)?;
                writer.write_u64::<BE>(*session_id)?;
                // Write original client address
                match original_client {
                    Some(addr) => {
                        writer.write_u8(1)?; // Has original client
                        match addr {
                            std::net::SocketAddr::V4(v4) => {
                                writer.write_u8(4)?; // IPv4
                                writer.write_all(&v4.ip().octets())?;
                                writer.write_u16::<BE>(v4.port())?;
                            }
                            std::net::SocketAddr::V6(v6) => {
                                writer.write_u8(6)?; // IPv6
                                writer.write_all(&v6.ip().octets())?;
                                writer.write_u16::<BE>(v6.port())?;
                            }
                        }
                    }
                    None => {
                        writer.write_u8(0)?; // No original client
                    }
                }
                // Write payload length and data
                writer.write_u32::<BE>(
                    payload
                        .len()
                        .try_into()
                        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "payload too long"))?,
                )?;
                writer.write_all(payload)?;
            }

            UnorderedLinkMsg::LinkStatus { link_id, status } => {
                writer.write_u8(Self::MSG_LINK_STATUS)?;
                writer.write_u128::<BE>(link_id.0)?;
                status.write(&mut writer)?;
            }

            UnorderedLinkMsg::Goodbye { reason } => {
                writer.write_u8(Self::MSG_GOODBYE)?;
                writer.write_u16::<BE>(
                    reason
                        .len()
                        .try_into()
                        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "reason too long"))?,
                )?;
                writer.write_all(reason.as_bytes())?;
            }
        }
        Ok(())
    }

    /// Deserializes a message from bytes.
    pub fn read(mut reader: impl io::Read) -> Result<Self, io::Error> {
        let msg_type = reader.read_u8()?;

        match msg_type {
            Self::MSG_WELCOME => {
                let mut magic = [0u8; 5];
                reader.read_exact(&mut magic)?;
                if magic != *Self::MAGIC {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid magic"));
                }

                let version = reader.read_u8()?;
                if version != Self::PROTOCOL_VERSION {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported version"));
                }

                let link_id = LinkId(reader.read_u128::<BE>()?);
                let cfg = UnorderedCfg::read(&mut reader)?;

                Ok(UnorderedLinkMsg::Welcome { cfg, link_id })
            }

            Self::MSG_CONNECT => {
                let mut magic = [0u8; 5];
                reader.read_exact(&mut magic)?;
                if magic != *Self::MAGIC {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid magic"));
                }

                let version = reader.read_u8()?;
                if version != Self::PROTOCOL_VERSION {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported version"));
                }

                let group_id = reader.read_u64::<BE>()?;
                let user_data_len = reader.read_u16::<BE>()? as usize;
                let mut user_data = vec![0u8; user_data_len];
                reader.read_exact(&mut user_data)?;
                let cfg = UnorderedCfg::read(&mut reader)?;

                Ok(UnorderedLinkMsg::Connect { group_id, user_data, cfg })
            }

            Self::MSG_ACCEPTED => {
                let link_id = LinkId(reader.read_u128::<BE>()?);
                let cfg = UnorderedCfg::read(&mut reader)?;
                Ok(UnorderedLinkMsg::Accepted { link_id, cfg })
            }

            Self::MSG_REFUSED => {
                let reason = RefusedReason::read(&mut reader)?;
                Ok(UnorderedLinkMsg::Refused { reason })
            }

            Self::MSG_PING => {
                let timestamp = reader.read_u64::<BE>()?;
                Ok(UnorderedLinkMsg::Ping { timestamp })
            }

            Self::MSG_PONG => {
                let timestamp = reader.read_u64::<BE>()?;
                Ok(UnorderedLinkMsg::Pong { timestamp })
            }

            Self::MSG_DATA => {
                let link_id = LinkId(reader.read_u128::<BE>()?);
                let session_id = reader.read_u64::<BE>()?;
                let original_client = {
                    let has_addr = reader.read_u8()? != 0;
                    if has_addr {
                        let addr_type = reader.read_u8()?;
                        let ip = match addr_type {
                            4 => {
                                let mut octets = [0u8; 4];
                                reader.read_exact(&mut octets)?;
                                std::net::IpAddr::from(octets)
                            }
                            6 => {
                                let mut octets = [0u8; 16];
                                reader.read_exact(&mut octets)?;
                                std::net::IpAddr::from(octets)
                            }
                            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown address type")),
                        };
                        let port = reader.read_u16::<BE>()?;
                        Some(std::net::SocketAddr::new(ip, port))
                    } else {
                        None
                    }
                };
                // Read payload
                let payload_len = reader.read_u32::<BE>()? as usize;
                let mut payload = vec![0u8; payload_len];
                reader.read_exact(&mut payload)?;
                Ok(UnorderedLinkMsg::Data { link_id, session_id, original_client, payload })
            }

            Self::MSG_LINK_STATUS => {
                let link_id = LinkId(reader.read_u128::<BE>()?);
                let status = LinkStatus::read(&mut reader)?;
                Ok(UnorderedLinkMsg::LinkStatus { link_id, status })
            }

            Self::MSG_GOODBYE => {
                let reason_len = reader.read_u16::<BE>()? as usize;
                let mut reason_bytes = vec![0u8; reason_len];
                reader.read_exact(&mut reason_bytes)?;
                let reason = String::from_utf8(reason_bytes)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-8 in reason"))?;
                Ok(UnorderedLinkMsg::Goodbye { reason })
            }

            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "unknown message type")),
        }
    }
}

impl RefusedReason {
    fn write(&self, mut writer: impl io::Write) -> Result<(), io::Error> {
        let code = match self {
            RefusedReason::GroupNotFound => 1,
            RefusedReason::GroupFull => 2,
            RefusedReason::IncompatibleConfig => 3,
            RefusedReason::ServerShutdown => 4,
            RefusedReason::AuthFailed => 5,
            RefusedReason::RateLimited => 6,
        };
        writer.write_u8(code)?;
        Ok(())
    }

    fn read(mut reader: impl io::Read) -> Result<Self, io::Error> {
        let code = reader.read_u8()?;
        match code {
            1 => Ok(RefusedReason::GroupNotFound),
            2 => Ok(RefusedReason::GroupFull),
            3 => Ok(RefusedReason::IncompatibleConfig),
            4 => Ok(RefusedReason::ServerShutdown),
            5 => Ok(RefusedReason::AuthFailed),
            6 => Ok(RefusedReason::RateLimited),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "unknown refused reason")),
        }
    }
}

impl LinkStatus {
    fn write(&self, mut writer: impl io::Write) -> Result<(), io::Error> {
        let code = match self {
            LinkStatus::Active => 1,
            LinkStatus::Blocked => 2,
            LinkStatus::Slow => 3,
            LinkStatus::Disconnecting => 4,
        };
        writer.write_u8(code)?;
        Ok(())
    }

    fn read(mut reader: impl io::Read) -> Result<Self, io::Error> {
        let code = reader.read_u8()?;
        match code {
            1 => Ok(LinkStatus::Active),
            2 => Ok(LinkStatus::Blocked),
            3 => Ok(LinkStatus::Slow),
            4 => Ok(LinkStatus::Disconnecting),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "unknown link status")),
        }
    }
}

// We need to implement serialization for UnorderedCfg
impl UnorderedCfg {
    /// Writes the configuration to a writer.
    pub(crate) fn write(&self, mut writer: impl io::Write) -> Result<(), io::Error> {
        writer.write_u32::<BE>(self.send_queue.get() as u32)?;
        writer.write_u32::<BE>(self.recv_queue.get() as u32)?;

        let strategy_code = match self.load_balance {
            crate::unordered_cfg::LoadBalanceStrategy::PacketRoundRobin => 1,
            crate::unordered_cfg::LoadBalanceStrategy::WeightedByBandwidth => 2,
            crate::unordered_cfg::LoadBalanceStrategy::FastestFirst => 3,
            crate::unordered_cfg::LoadBalanceStrategy::WeightedByPacketLoss => 4,
            crate::unordered_cfg::LoadBalanceStrategy::DynamicAdaptive => 5,
        };
        writer.write_u8(strategy_code)?;

        writer.write_u64::<BE>(self.heartbeat_interval.as_millis() as u64)?;
        writer.write_u64::<BE>(self.link_timeout.as_millis() as u64)?;
        writer.write_u32::<BE>(self.max_packet_size as u32)?;
        writer.write_u32::<BE>(self.connect_queue.get() as u32)?;

        writer.write_u8(self.stats_intervals.len() as u8)?;
        for interval in &self.stats_intervals {
            writer.write_u64::<BE>(interval.as_millis() as u64)?;
        }

        // Write node_role
        let role_code = match self.node_role {
            NodeRole::Client => 1,
            NodeRole::Server => 2,
            NodeRole::Balanced => 3,
        };
        writer.write_u8(role_code)?;

        Ok(())
    }

    /// Reads the configuration from a reader.
    pub(crate) fn read(mut reader: impl io::Read) -> Result<Self, io::Error> {
        use std::num::NonZeroUsize;
        use std::time::Duration;

        let send_queue = NonZeroUsize::new(reader.read_u32::<BE>()? as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid send_queue"))?;
        let recv_queue = NonZeroUsize::new(reader.read_u32::<BE>()? as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid recv_queue"))?;

        let strategy_code = reader.read_u8()?;
        let load_balance = match strategy_code {
            1 => crate::unordered_cfg::LoadBalanceStrategy::PacketRoundRobin,
            2 => crate::unordered_cfg::LoadBalanceStrategy::WeightedByBandwidth,
            3 => crate::unordered_cfg::LoadBalanceStrategy::FastestFirst,
            4 => crate::unordered_cfg::LoadBalanceStrategy::WeightedByPacketLoss,
            5 => crate::unordered_cfg::LoadBalanceStrategy::DynamicAdaptive,
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown load balance strategy")),
        };

        let heartbeat_interval = Duration::from_millis(reader.read_u64::<BE>()?);
        let link_timeout = Duration::from_millis(reader.read_u64::<BE>()?);
        let max_packet_size = reader.read_u32::<BE>()? as usize;
        let connect_queue = NonZeroUsize::new(reader.read_u32::<BE>()? as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid connect_queue"))?;

        let intervals_count = reader.read_u8()?;
        let mut stats_intervals = Vec::new();
        for _ in 0..intervals_count {
            let interval = Duration::from_millis(reader.read_u64::<BE>()?);
            stats_intervals.push(interval);
        }

        // Read node_role
        let role_code = reader.read_u8()?;
        let node_role = match role_code {
            1 => NodeRole::Client,
            2 => NodeRole::Server,
            3 => NodeRole::Balanced,
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid node_role")),
        };

        Ok(UnorderedCfg {
            send_queue,
            recv_queue,
            load_balance,
            heartbeat_interval,
            link_timeout,
            max_packet_size,
            connect_queue,
            stats_intervals,
            node_role,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_message_roundtrip() {
        let messages = vec![
            UnorderedLinkMsg::Welcome { cfg: UnorderedCfg::default(), link_id: LinkId(42) },
            UnorderedLinkMsg::Connect {
                group_id: 12345,
                user_data: b"test data".to_vec(),
                cfg: UnorderedCfg::default(),
            },
            UnorderedLinkMsg::Accepted { link_id: LinkId(99), cfg: UnorderedCfg::default() },
            UnorderedLinkMsg::Refused { reason: RefusedReason::GroupFull },
            UnorderedLinkMsg::Ping { timestamp: 1234567890 },
            UnorderedLinkMsg::Pong { timestamp: 1234567890 },
            UnorderedLinkMsg::Data {
                link_id: LinkId(5),
                session_id: 0,
                original_client: None,
                payload: b"test data".to_vec(),
            },
            UnorderedLinkMsg::LinkStatus { link_id: LinkId(7), status: LinkStatus::Slow },
            UnorderedLinkMsg::Goodbye { reason: "test shutdown".to_string() },
        ];

        for original in messages {
            let mut buffer = Vec::new();
            original.write(&mut buffer).unwrap();

            let mut cursor = Cursor::new(buffer);
            let decoded = UnorderedLinkMsg::read(&mut cursor).unwrap();

            assert_eq!(original, decoded);
        }
    }

    #[test]
    fn test_config_roundtrip() {
        let configs = vec![
            UnorderedCfg::default(),
            UnorderedCfg::low_latency(),
            UnorderedCfg::high_throughput(),
            UnorderedCfg::unreliable_network(),
        ];

        for original in configs {
            let mut buffer = Vec::new();
            original.write(&mut buffer).unwrap();

            let mut cursor = Cursor::new(buffer);
            let decoded = UnorderedCfg::read(&mut cursor).unwrap();

            assert_eq!(original, decoded);
        }
    }
}
