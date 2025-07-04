use std::{
    io::Result,
    net::SocketAddr,
    sync::Arc,
};

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};
use tokio_kcp::{KcpConfig, KcpListener};

use aggligator::{
    io::IoBox,
    transport::{AcceptedStreamBox, AcceptingTransport},
};

use crate::{link_tag::KcpLinkTag, NAME};

/// KCP transport for incoming connections.
///
/// This transport is stream-based.
#[derive(Debug)]
pub struct KcpAcceptor {
    listeners: Vec<Arc<Mutex<KcpListener>>>,
    cfg: KcpConfig,
}

impl KcpAcceptor {
    /// Create a new KCP transport listening for incoming connections.
    ///
    /// It listens on the local addresses specified in `addrs`.
    pub async fn new(cfg: &KcpConfig, addrs: impl IntoIterator<Item = SocketAddr>) -> Result<Self> {
        let mut listeners = Vec::new();

        for addr in addrs {
            let listener = KcpListener::bind(*cfg, addr).await?;
            listeners.push(Arc::new(Mutex::new(listener)));
        }

        if listeners.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "at least one listener is required",
            ));
        }

        Ok(Self {
            listeners,
            cfg: *cfg,
        })
    }

    /// Gets the KCP configuration.
    pub fn config(&self) -> &KcpConfig {
        &self.cfg
    }
}

#[async_trait]
impl AcceptingTransport for KcpAcceptor {
    fn name(&self) -> &str {
        NAME
    }

    async fn listen(&self, tx: mpsc::Sender<AcceptedStreamBox>) -> Result<()> {
        for listener in &self.listeners {
            let listener = listener.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                loop {
                    // 单次加锁，避免高并发自旋
                    let mut guard = listener.lock().await;
                    let (stream, peer) = match guard.accept().await {
                        Ok((stream, peer)) => (stream, peer),
                        Err(e) => {
                            tracing::error!("Error accepting KCP connection: {}", e);
                            continue;
                        }
                    };
                    let local = match guard.local_addr() {
                        Ok(addr) => addr,
                        Err(e) => {
                            tracing::error!("Error getting local address: {}", e);
                            continue;
                        }
                    };
                    drop(guard); // 及时释放

                    tracing::debug!("Accepted KCP connection from {} to {}", peer, local);

                    let tag = KcpLinkTag::incoming(local, peer);
                    let (r, w) = tokio::io::split(stream);
                    
                    let stream_box = AcceptedStreamBox::new(IoBox::new(r, w).into(), tag);
                    if let Err(e) = tx.send(stream_box).await {
                        tracing::error!("Failed to send accepted stream: {}", e);
                        break;
                    }
                }
            });
        }
        Ok(())
    }
}
