use bytes::Bytes;
use std::io::Result;
use tokio::{net::UdpSocket, sync::mpsc};

/// Task aggregating multiple UDP sockets without reordering.
#[derive(Debug)]
pub struct UdpTask {
    send_txs: Vec<mpsc::Sender<Bytes>>,
    write_rx: mpsc::Receiver<Bytes>,
    weights: Vec<f64>,
    deficits: Vec<f64>,
}

async fn link_task(mut socket: UdpSocket, mut tx: mpsc::Sender<Bytes>, mut rx: mpsc::Receiver<Bytes>) {
    let mut buf = vec![0u8; 2048];
    loop {
        tokio::select! {
            res = socket.recv(&mut buf) => {
                match res {
                    Ok(len) => {
                        let _ = tx.send(Bytes::copy_from_slice(&buf[..len])).await;
                    }
                    Err(_) => break,
                }
            }
            Some(data) = rx.recv() => {
                let _ = socket.send(&data).await;
            }
            else => break,
        }
    }
}

impl UdpTask {
    /// Creates a new task handling the provided sockets using equal weights.
    pub fn new(sockets: Vec<UdpSocket>, read_tx: mpsc::Sender<Bytes>, write_rx: mpsc::Receiver<Bytes>) -> Self {
        Self::with_weights(sockets, Vec::new(), read_tx, write_rx)
    }

    /// Creates a new task with specified send weights for each socket.
    pub fn with_weights(
        sockets: Vec<UdpSocket>, weights: Vec<f64>, read_tx: mpsc::Sender<Bytes>, write_rx: mpsc::Receiver<Bytes>,
    ) -> Self {
        let count = sockets.len();
        let mut send_txs = Vec::new();
        for sock in sockets {
            let (tx_to_link, rx_from_main) = mpsc::channel(1024);
            let tx_from_link = read_tx.clone();
            tokio::spawn(link_task(sock, tx_from_link, rx_from_main));
            send_txs.push(tx_to_link);
        }
        let mut weights = if weights.len() == count { weights } else { vec![1.0; count] };
        for w in &mut weights {
            if *w <= 0.0 {
                *w = 1.0;
            }
        }
        Self { deficits: vec![0.0; count], weights, send_txs, write_rx }
    }

    /// Runs the task until the write channel is closed.
    pub async fn run(mut self) -> Result<()> {
        if self.send_txs.is_empty() {
            return Ok(());
        }
        while let Some(data) = self.write_rx.recv().await {
            for (d, w) in self.deficits.iter_mut().zip(&self.weights) {
                *d += *w;
            }
            if let Some((idx, _)) = self
                .deficits
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            {
                self.deficits[idx] -= 1.0;
                let _ = self.send_txs[idx].send(data).await;
            }
        }
        Ok(())
    }

    /// Updates the weight of the specified link.
    pub fn set_weight(&mut self, idx: usize, weight: f64) {
        if let Some(w) = self.weights.get_mut(idx) {
            *w = weight.max(0.0);
        }
    }
}
