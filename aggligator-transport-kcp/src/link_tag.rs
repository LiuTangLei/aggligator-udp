use std::{
    any::Any,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    net::SocketAddr,
};

use aggligator::{
    control::Direction,
    transport::{LinkTag, LinkTagBox},
};

use crate::NAME;

/// Link tag for KCP link.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KcpLinkTag {
    /// Local address.
    pub local: SocketAddr,
    /// Remote address.
    pub remote: SocketAddr,
    /// Link direction.
    pub direction: Direction,
}

impl fmt::Display for KcpLinkTag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let dir = match self.direction {
            Direction::Incoming => "<-",
            Direction::Outgoing => "->",
        };
        write!(f, "KCP {dir} {} (local: {})", self.remote, self.local)
    }
}

impl KcpLinkTag {
    /// Creates a new link tag for a KCP link.
    pub fn new(local: SocketAddr, remote: SocketAddr, direction: Direction) -> Self {
        Self { local, remote, direction }
    }

    /// Creates a new incoming link tag.
    pub fn incoming(local: SocketAddr, remote: SocketAddr) -> Self {
        Self::new(local, remote, Direction::Incoming)
    }

    /// Creates a new outgoing link tag.
    pub fn outgoing(local: SocketAddr, remote: SocketAddr) -> Self {
        Self::new(local, remote, Direction::Outgoing)
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
        Vec::new()
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
