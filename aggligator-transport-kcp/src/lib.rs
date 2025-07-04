#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    html_favicon_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    issue_tracker_base_url = "https://github.com/surban/aggligator/issues/"
)]

//! [Aggligator](aggligator) transport: KCP
//!
//! This provides KCP transport for the [Aggligator link aggregator].
//!
//! [Aggligator link aggregator]: https://crates.io/crates/aggligator

/// KCP acceptor implementation.
pub mod acceptor;
/// KCP connector implementation.
pub mod connector;
/// KCP link filter types.
pub mod filter;
/// KCP link tag implementation.
pub mod link_tag;
/// Utilities for KCP transport.
pub mod util;

pub use acceptor::KcpAcceptor;
pub use connector::KcpConnector;
pub use filter::KcpLinkFilter;
pub use link_tag::KcpLinkTag;
pub use util::IpVersion;

pub use tokio_kcp::KcpConfig;

static NAME: &str = "kcp";
