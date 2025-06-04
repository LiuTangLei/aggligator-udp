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
use aggligator::{UnorderedAggTask, UnorderedCfg};
use std::net::SocketAddr;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create UDP transport
    let transport = UdpTransport::new("127.0.0.1:8080".parse()?).await?;
    
    // Configure aggregation
    let config = UnorderedCfg::default();
    
    // Create aggregation task
    let agg_task = UnorderedAggTask::new(config, transport).await?;
    
    // Use the aggregation...
    
    Ok(())
}
```

## License

Licensed under the Apache License, Version 2.0.
