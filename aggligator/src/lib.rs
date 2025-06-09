#![cfg_attr(not(feature = "js"), forbid(unsafe_code))]
#![cfg_attr(feature = "js", deny(unsafe_code))]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    html_favicon_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    issue_tracker_base_url = "https://github.com/surban/aggligator/issues/"
)]

//! Aggregates multiple links into one connection.
//!
//! Aggligator takes multiple network links (for example [TCP] connections) between two
//! endpoints and combines them into one connection that has the combined bandwidth
//! of all links. Additionally it provides resiliency against failure of individual
//! links and allows adding and removing of links on-the-fly.
//!
//! It serves the same purpose as [Multipath TCP] and [SCTP] but works over existing,
//! widely adopted protocols such as TCP, HTTPS, TLS and WebSockets and is completely
//! implemented in user space without the need for any support from the operating system.
//!
//! Aggligator is written in 100% safe Rust and builds upon the [Tokio](tokio)
//! asynchronous runtime. It works on all major native platform as well as WebAssembly.
//!
//! [TCP]: https://en.wikipedia.org/wiki/Transmission_Control_Protocol
//! [Multipath TCP]: https://en.wikipedia.org/wiki/Multipath_TCP
//! [SCTP]: https://en.wikipedia.org/wiki/Stream_Control_Transmission_Protocol
//!
//! # Link requirements
//!
//! A link can either be stream-based (implementing the [AsyncRead] and [AsyncWrite] traits)
//! or packet-based (implementing the [Sink] and [Stream] traits).
//! In both cases the implementation of the link must ensure data integrity and deliver data
//! in the same order as it was sent.
//! If data has been lost or corrupted underway, the link must handle retransmission
//! and, if that is unsuccessful, fail by disconnecting itself.
//!
//! In the case of TCP this is handled by the operating system and thus
//! a [TcpStream] or protocols building on top of that (such as TLS or WebSockets)
//! can be directly used as links.
//!
//! Other then the requirements stated above, Aggligator makes no assumption about
//! the type of links and can work over any networking methodology such as
//! TCP/IP, Bluetooth, and serial links. It never interfaces directly with the
//! operating system and only uses links provided by the user.
//!
//! [AsyncRead]: tokio::io::AsyncRead
//! [AsyncWrite]: tokio::io::AsyncWrite
//! [Sink]: futures::sink::Sink
//! [Stream]: futures::stream::Stream
//! [TcpStream]: https://docs.rs/tokio/1/tokio/net/struct.TcpStream.html
//!
//! # Connection security
//!
//! Aggligator does *not* perform cryptographic authentication of the remote endpoint or encryption of data.
//! If you are sending sensitive data over an untrusted connection you should encrypt it
//! and authenticate the remote endpoint, for example using [TLS].
//! The implementation provided in the [tokio-rustls] crate works nicely with Aggligator.
//!
//! However, the unique identifier of each connection is encrypted using a shared
//! secret that is exchanged via [Diffie-Hellman key exchange].
//! Thus, an eavesdropper cannot inject fake links to an existing connection by using
//! the spoofed connection identifier.
//! This provides the same security level against insertion of malicious data and connection
//! termination by an adversary as you would have when using a single unencrypted
//! TCP connection.
//!
//! [TLS]: https://en.wikipedia.org/wiki/Transport_Layer_Security
//! [tokio-rustls]: https://docs.rs/tokio-rustls/
//! [Diffie-Hellman key exchange]: https://en.wikipedia.org/wiki/Diffie%E2%80%93Hellman_key_exchange
//!
//! # Basic usage and utility functions
//!
//! See the [connect module](mod@connect) on how to accept incoming connections
//! and establish outgoing connections. This is agnostic of the underlying protocol.
//!
//! Useful functions for working with TCP-based links, encryption and authentication using TLS,
//! can be found in the **[aggligator-transport-tcp]** crate. A
//! [collection of transports](https://crates.io/keywords/aggligator-transport)
//! for Aggligator is provided on crates.io.
//!
//! Command line utilites, that also serve as fully worked out examples, are provided in the
//! **[aggligator-util]** crate.
//!
//! [aggligator-transport-tcp]: https://docs.rs/aggligator-transport-tcp
//! [aggligator-util]: https://crates.io/crates/aggligator-util
//!

#[cfg(target_pointer_width = "16")]
compile_error!("target pointer width must be at least 32 bits");

#[doc(hidden)]
pub mod exec;

mod agg;
pub mod alc;
pub mod cfg;
pub mod connect;
pub mod control;
pub mod id;
pub mod io;
mod msg;
mod peekable_mpsc;
mod seq;
pub mod transport;
pub mod unordered_cfg;
pub mod unordered_msg;
pub mod unordered_task;

#[cfg(feature = "dump")]
#[cfg_attr(docsrs, doc(cfg(feature = "dump")))]
pub use agg::dump;

pub use agg::task::{Task, TaskError};

/// Link aggregator protocol error.
macro_rules! protocol_err {
    ($($t:tt)*) => {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!($($t)*))
    };
}

pub(crate) use protocol_err;

pub use cfg::Cfg;
pub use connect::{connect, Incoming, Listener, Outgoing, Server};
pub use control::{Control, Link};
pub use transport::{Acceptor, Connector};
pub use unordered_cfg::{LoadBalanceStrategy, UnorderedCfg};
pub use unordered_msg::UnorderedLinkMsg;
pub use unordered_task::{
    UnorderedAggStats, UnorderedAggTask, UnorderedControlMsg, UnorderedLinkState, UnorderedLinkTransport,
};
