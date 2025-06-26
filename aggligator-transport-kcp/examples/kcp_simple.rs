//! Simple KCP transport example

use aggligator_transport_kcp::{KcpAcceptor, KcpConnector};
use std::net::SocketAddr;
use tokio::time::{sleep, Duration};
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    info!("Starting KCP transport example");

    // Define addresses
    let listen_addr: SocketAddr = "127.0.0.1:8080".parse()?;
    let target_addr: SocketAddr = "127.0.0.1:8080".parse()?;

    // Create acceptor and connector
    let acceptor = KcpAcceptor::new([listen_addr]);
    let connector = KcpConnector::new([target_addr]);

    info!("Created KCP acceptor: {}", acceptor);
    info!("Created KCP connector: {}", connector);

    // In a real application, you would use these with Aggligator
    // For now, just show that they can be created successfully
    
    info!("KCP transport components created successfully!");
    info!("This example demonstrates basic KCP transport creation.");
    info!("To use with Aggligator, add these to your transport configuration.");
    
    // Sleep a bit to show the program works
    sleep(Duration::from_millis(100)).await;
    
    Ok(())
}
