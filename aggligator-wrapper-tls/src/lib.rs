#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    html_favicon_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    issue_tracker_base_url = "https://github.com/surban/aggligator/issues/"
)]

//! [Aggligator](aggligator) transport wrapper using TLS
//!
//! This provides connection security by wrapping Aggligator links
//! in TLS.

use async_trait::async_trait;
use std::{io::Result, sync::Arc};
use tokio::io::split;
use tokio_rustls::{TlsAcceptor, TlsConnector};

#[doc(no_inline)]
pub use rustls::{pki_types::ServerName, ClientConfig, RootCertStore, ServerConfig};

use aggligator::{
    io::{IoBox, StreamBox},
    transport::{AcceptingWrapper, ConnectingWrapper},
};

static NAME: &str = "tls";

/// TLS outgoing connection wrapper.
///
/// Only IO-based streams are supported.
///
/// Pass this to [`Connector::wrapped`](aggligator::transport::Connector::wrapped) to apply TLS
/// encryption to each outgoing link.
///
/// # Panics
/// Panics if a packet-based stream is supplied.
#[derive(Debug)]
#[must_use = "you must pass this wrapper to the connector"]
pub struct TlsClient {
    server_name: ServerName<'static>,
    client_cfg: Arc<ClientConfig>,
}

impl TlsClient {
    /// Creates a new TLS outgoing connection wrapper.
    ///
    /// The identity of the server is verified using TLS against `server_name`.
    /// The outgoing link is encrypted using TLS with the configuration specified
    /// in `client_cfg`.
    pub fn new(client_cfg: Arc<ClientConfig>, server_name: ServerName<'static>) -> Self {
        Self { server_name, client_cfg }
    }
}

#[async_trait]
impl ConnectingWrapper for TlsClient {
    fn name(&self) -> &str {
        NAME
    }

    async fn wrap(&self, stream: StreamBox) -> Result<StreamBox> {
        let StreamBox::Io(io) = stream else { panic!("TlsClient only supports IO-based streams") };
        let connector = TlsConnector::from(self.client_cfg.clone());
        let tls = connector.connect(self.server_name.clone(), io).await?;
        let (rh, wh) = split(tls);
        Ok(IoBox::new(rh, wh).into())
    }
}

/// TLS incoming connection wrapper.
///
/// Only IO-based streams are supported.
///
/// # Panics
/// Panics if a packet-based stream is supplied.
#[derive(Debug)]
#[must_use = "you must pass this wrapper to the acceptor"]
pub struct TlsServer {
    server_cfg: Arc<ServerConfig>,
}

impl TlsServer {
    /// Creates a new TLS incoming connection wrapper.
    ///
    /// Incoming links are encrypted using TLS with the configuration specified
    /// in `server_cfg`.
    pub fn new(server_cfg: Arc<ServerConfig>) -> Self {
        Self { server_cfg }
    }
}

#[async_trait]
impl AcceptingWrapper for TlsServer {
    fn name(&self) -> &str {
        NAME
    }

    async fn wrap(&self, stream: StreamBox) -> Result<StreamBox> {
        let StreamBox::Io(io) = stream else { panic!("TlsServer only supports IO-based streams") };
        let acceptor = TlsAcceptor::from(self.server_cfg.clone());
        let tls = acceptor.accept(io).await?;
        let (rh, wh) = split(tls);
        Ok(IoBox::new(rh, wh).into())
    }
}
