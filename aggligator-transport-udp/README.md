# Aggligator UDP Transport

UDP transport implementation for Aggligator unordered aggregation.

## Overview

This crate provides UDP socket implementation of the `UnorderedLinkTransport` trait from the aggligator crate, enabling efficient unordered packet aggregation over UDP connections.

## Features

- Asynchronous UDP socket operations
- Link health monitoring and statistics
- Automatic retry and connection management
- Integration with Aggligator's unordered aggregation framework
- Support for multiple concurrent UDP links

## Usage

```rust
use aggligator_transport_udp::UdpTransport;
use aggligator::{UnorderedAggTask, UnorderedCfg}; // Ensure these types are correctly imported
use std::net::{SocketAddr, ToSocketAddrs}; // Added ToSocketAddrs for convenience
use std::io;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Example: Client wants to connect to a server
    let server_address_str = "127.0.0.1:5801"; // Example target server address
    let server_addr = server_address_str.to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("Could not resolve address: {}", server_address_str)))?;

    // Create a connected UDP transport for a client link.
    // Option 1: Let the OS choose the local IP and port (common for clients).
    let transport = UdpTransport::new_unbound(server_addr).await?;
    
    // Option 2: Bind to a specific local address and then connect.
    // let local_bind_addr: SocketAddr = "0.0.0.0:0".parse()?; // OS assigns port
    // let transport = UdpTransport::connect(local_bind_addr, server_addr).await?;
    
    println!("UDP transport established: Local {} -> Remote {}", transport.local_addr(), transport.remote_addr());
    
    // Configure aggregation (this is a basic example; real configuration might be more detailed)
    let config = UnorderedCfg::default();
    
    // Create aggregation task using the connected transport.
    // This assumes UnorderedAggTask is designed to work with a single, pre-connected link transport.
    // If it manages multiple links or complex setup, its API might differ.
    match UnorderedAggTask::new(config, transport).await {
        Ok(_agg_task) => {
            println!("Aggregation task started successfully.");
            // Here you would typically use the _agg_task to send/receive data.
            // For example: _agg_task.send_packet(b"hello from client").await?;
        }
        Err(e) => {
            eprintln!("Failed to create aggregation task: {}", e);
            return Err(Box::new(e)); // Propagate the error
        }
    }
        
    Ok(())
}
```

## License

Licensed under the Apache License, Version 2.0.
