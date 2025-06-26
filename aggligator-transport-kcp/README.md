# Aggligator transport: KCP

KCP transport for [Aggligator](https://github.com/surban/aggligator), providing reliable, low-latency network connections.

## Overview

This crate implements KCP (ARQ protocol) transport for Aggligator link aggregation. KCP provides:

- **Low latency**: Optimized for real-time applications
- **Reliable delivery**: Built-in ARQ with configurable parameters
- **Congestion control**: Adaptive transmission based on network conditions
- **Fast recovery**: Quick retransmission on packet loss

## Features

- Async/await support with Tokio
- Configurable KCP parameters (window size, intervals, etc.)
- Integration with Aggligator's link aggregation
- Health monitoring and statistics

## Usage

```rust
use aggligator_transport_kcp::connect;
use std::net::SocketAddr;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let remote: SocketAddr = "127.0.0.1:8080".parse()?;
    let local: SocketAddr = "0.0.0.0:0".parse()?;

    let (tx, rx) = connect(local, remote).await?;
    // Use tx/rx with Aggligator...

    Ok(())
}
```

## License

Licensed under the Apache License, Version 2.0.
