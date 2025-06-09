# agg-udp: UDP Aggregation Tool

## Overview

`agg-udp` is a high-performance UDP packet aggregation tool that provides unordered packet aggregation across multiple UDP links. It's designed for maximum throughput without the overhead of ordering or reliability guarantees.

## Features

- **Multi-link UDP aggregation**: Aggregate traffic across multiple UDP connections
- **Load balancing strategies**: Support for round-robin, random, weighted, and fastest link selection
- **Performance monitoring**: Real-time statistics and throughput monitoring
- **Protocol-agnostic**: Works with any UDP-based application traffic
- **High throughput**: Optimized for maximum packet processing speed

## Installation

Build from source:

```bash
cargo build --release --bin agg-udp
```

## Usage

### Basic Syntax

```bash
agg-udp [OPTIONS] <COMMAND>
```

### Global Options

- `--cfg <FILE>`: Configuration file for advanced settings (e.g., `example-udp-config.json` or a TOML file).
- `--verbose, -v`: Enable verbose debug logging.
- `--stats-interval <SECONDS>`: Statistics reporting interval in seconds (default: 5).

### Commands

#### 1. Server Mode

Run a UDP aggregation server:

```bash
agg-udp server [OPTIONS]
```

**Options:**
- `--bind <ADDR>`: Server bind address (default: `0.0.0.0:5801`). This is the address `agg-udp` listens on for incoming client connections.
- `--target <ADDR>`: Target address to forward aggregated traffic to (e.g., your application server like `127.0.0.1:8080`).
- `--links <COUNT>`: Number of server links (logical pathways for aggregation) to create (default: 4). This doesn't directly translate to the number of UDP sockets opened by the server for listening, but rather to the capacity of the aggregation logic.
- `--buffer-size <SIZE>`: UDP socket receive/send buffer size in bytes (default: 65536). Adjust based on expected traffic and system capabilities.

**Example:**
```bash
# Start server on port 5801, forward aggregated traffic to localhost:8080, with 6 logical links
agg-udp server --bind 0.0.0.0:5801 --target 127.0.0.1:8080 --links 6
```

#### 2. Client Mode

Run a UDP aggregation client:

```bash
agg-udp client [OPTIONS]
```

**Options:**
- `--local <ADDR>`: Local bind address for the client's outgoing connections (e.g., `127.0.0.1:0` to let the OS choose a port). Can be a single address or multiple for multi-link from distinct local endpoints.
- `--remote <ADDR>`: Remote server addresses. Specify one or more `agg-udp` server addresses (e.g., `192.168.1.10:5801`). Each `--remote` creates a link.
- `--target <ADDR>`: Local address that the client listens on and forwards received aggregated traffic to (e.g. `127.0.0.1:3000` where your local application listens).
- `--strategy <STRATEGY>`: Load balancing strategy for distributing packets across multiple remote links.
  - `round-robin`: Distribute packets sequentially across available links.
  - `random`: Randomly select a link for each packet.
  - `weighted`: Distribute packets based on configured weights for each link (requires further configuration in the config file).
  - `fastest`: Send packets to the link with the lowest observed latency (experimental, may require specific health check setup).
- `--timeout <MS>`: Connection timeout in milliseconds for establishing links (default: 5000).
- `--health-check <MS>`: Interval in milliseconds for sending health checks to maintain link status (default: 1000).

**Example:**
```bash
# Client listens on 127.0.0.1:3000 for local application traffic,
# connects to three remote agg-udp servers, and uses round-robin.
agg-udp client --target 127.0.0.1:3000 \
               --remote 192.168.1.10:5801 \
               --remote 192.168.1.11:5801 \
               --remote 192.168.1.12:5801 \
               --strategy round-robin
```

#### 3. Test Mode

Run performance and functionality tests:

```bash
agg-udp test [OPTIONS]
```

**Options:**
- `--server <ADDR>`: Target server address (default: 127.0.0.1:5801)
- `--duration <SECONDS>`: Test duration in seconds (default: 30)
- `--packet-size <SIZE>`: Test packet size in bytes (default: 1024)
- `--rate <PPS>`: Target packet rate (packets per second, default: 1000)
- `--links <COUNT>`: Number of test links (default: 4)
- `--parallel`: Run tests in parallel across all links

**Example:**
```bash
# Run 60-second performance test with 1KB packets at 5000 PPS
agg-udp test --server 192.168.1.10:5801 \
             --duration 60 \
             --packet-size 1024 \
             --rate 5000 \
             --links 8 \
             --parallel
```

## Configuration File

Advanced settings can be specified in a JSON or TOML configuration file. 
`agg-udp` will try to load `example-udp-config.json` if present, or you can specify one with `--cfg`.

**Example JSON (`example-udp-config.json`):**
```json
{
  "network": {
    "send_buffer_size": 1048576,
    "recv_buffer_size": 1048576,
    "connect_timeout_ms": 3000,
    "health_check_interval_ms": 500
  },
  "aggregation": {
    "strategy": "round-robin", // "random", "weighted", "fastest"
    "max_links": 16,
    "min_healthy_links": 1 // Adjusted from 2 for typical single server scenarios
  },
  "performance": {
    "stats_interval_seconds": 5,
    "enable_detailed_stats": true,
    "batch_size": 32,
    "worker_threads": 4 // Number of Tokio worker threads
  },
  "logging": {
    "level": "info", // "debug", "warn", "error"
    "enable_timestamps": true
  }
}
```

Use with: `agg-udp --cfg my-config.json <command>` or `agg-udp --cfg my-config.toml <command>`

## Usage Scenarios

### 1. High-Throughput Data Transfer (Client to Server)

**Scenario**: A client application sends a large volume of UDP data to a server application, using `agg-udp` to potentially bond multiple network paths or improve resilience.

**Setup**:

*   **Server Side (`agg-udp` server):** Listens for aggregated UDP traffic from the `agg-udp` client and forwards it to the actual server application.
    ```bash
    # agg-udp server listens on 0.0.0.0:5801
    # Forwards received (de-aggregated) packets to the actual server app at 127.0.0.1:8080
    agg-udp server --bind 0.0.0.0:5801 --target 127.0.0.1:8080 --links 8
    ```

*   **Client Side (`agg-udp` client):** Listens for UDP traffic from the local client application, aggregates it, and sends it over one or more links to the `agg-udp` server.
    ```bash
    # Local client app sends UDP data to 127.0.0.1:3000
    # agg-udp client listens on 127.0.0.1:3000
    # Connects to agg-udp server(s) (e.g., server1:5801, server2:5801)
    # Uses 'fastest' link strategy
    agg-udp client --target 127.0.0.1:3000 \
                   --remote server1.example.com:5801 \
                   --remote server2.example.com:5801 \
                   --strategy fastest
    ```
Your client application would then be configured to send its UDP packets to `127.0.0.1:3000`.
Your server application would listen on `127.0.0.1:8080`.

### 2. Load Balancing and Redundancy (Client to Multiple Servers)

**Scenario**: A client application needs to distribute its UDP traffic across multiple instances of a server application, potentially hosted on different machines, for load balancing and redundancy. `agg-udp` client connects to multiple `agg-udp` servers, each forwarding to a backend application server.

**Setup**:

*   **Server Side (multiple `agg-udp` servers, each fronting an app server instance):**
    *   `agg-udp-server-1` on `192.168.1.10` forwarding to `app-server-1` on `127.0.0.1:8080` (local to `192.168.1.10`)
        ```bash
        # On machine 192.168.1.10
        agg-udp server --bind 0.0.0.0:5801 --target 127.0.0.1:8080
        ```
    *   `agg-udp-server-2` on `192.168.1.11` forwarding to `app-server-2` on `127.0.0.1:8080` (local to `192.168.1.11`)
        ```bash
        # On machine 192.168.1.11
        agg-udp server --bind 0.0.0.0:5801 --target 127.0.0.1:8080
        ```
    *   `agg-udp-server-3` on `192.168.1.12` forwarding to `app-server-3` on `127.0.0.1:8080` (local to `192.168.1.12`)
        ```bash
        # On machine 192.168.1.12
        agg-udp server --bind 0.0.0.0:5801 --target 127.0.0.1:8080
        ```

*   **Client Side (`agg-udp` client):** Listens for local application traffic and distributes it across the remote `agg-udp` servers.
    ```bash
    # Local client app sends UDP data to 127.0.0.1:3000
    # agg-udp client listens on 127.0.0.1:3000
    # Connects to the three agg-udp servers with weighted load balancing (weights configured in JSON/TOML)
    agg-udp client --target 127.0.0.1:3000 \
                   --remote 192.168.1.10:5801 \
                   --remote 192.168.1.11:5801 \
                   --remote 192.168.1.12:5801 \
                   --strategy weighted --cfg my-weighted-config.json 
    ```
    (Assuming `my-weighted-config.json` defines the weights for the links).

### 3. Performance Testing

**Scenario**: Test the capacity and performance of a UDP-based service or network path.

**Setup**:

*   **Server Side (`agg-udp` server):** Set up to receive and process test packets.
    ```bash
    # agg-udp server setup for testing
    agg-udp server --bind 0.0.0.0:5801 --target 127.0.0.1:8080 --links 4
    ```

*   **Client Side (`agg-udp` client in test mode):** Configured to send a specific rate of packets for a defined duration.
    ```bash
    # Performance test with 1KB packets at 5000 PPS for 60 seconds
    agg-udp test --server 192.168.1.10:5801 \
                 --duration 60 \
                 --packet-size 1024 \
                 --rate 5000 \
                 --links 4 \
                 --parallel
    ```

## Performance Monitoring

The tool provides real-time statistics including:

- **Throughput**: Packets per second and bytes per second
- **Link Status**: Health and performance of individual links
- **Load Distribution**: How traffic is distributed across links
- **Error Rates**: Packet loss and connection errors
- **Latency**: Response times for health checks

Example output:
```
[2024-01-15 10:30:15] UDP Aggregation Stats: // Note: Timestamp is illustrative
  Total Links: 4 (4 healthy, 0 failed)
  Throughput: 15,432 pps, 23.4 MB/s
  Client Target: 127.0.0.1:3000 (if in client mode)
  Server Target: 127.0.0.1:8080 (if in server mode)
  Load Distribution:
    Link 1 (192.168.1.10:5801): 25.2% - 3,889 pps, 5.9 MB/s
    Link 2 (192.168.1.11:5801): 24.8% - 3,826 pps, 5.8 MB/s  
    Link 3 (192.168.1.12:5801): 25.1% - 3,871 pps, 5.9 MB/s
    Link 4 (192.168.1.13:5801): 24.9% - 3,846 pps, 5.8 MB/s
  Average Latency: 2.3ms
  Error Rate: 0.01%
```

## Architecture

The tool is built on the Aggligator framework and consists of:

1. **Unordered Aggregation Core**: Protocol-agnostic packet aggregation provided by the `aggligator` crate.
2. **UDP Transport Layer**: UDP-specific networking implementation using `aggligator-transport-udp`.
3. **Command Line Interface**: Parsing arguments, configuring, and running server/client/test modes.
4. **Load Balancing**: Multiple strategies for traffic distribution across links.
5. **Health Monitoring**: Link health detection and failover.
6. **Statistics Engine**: Real-time performance monitoring.

## Troubleshooting

### Common Issues

1. **Port binding errors**: Ensure the specified ports are available
2. **High packet loss**: Check network capacity and buffer sizes
3. **Connection timeouts**: Adjust timeout values for high-latency networks
4. **Performance issues**: Tune buffer sizes and worker thread counts

### Debug Mode

Enable verbose logging for troubleshooting:
```bash
agg-udp --verbose <command>
```

### Performance Tuning

- Increase UDP buffer sizes for high-throughput scenarios
- Adjust the number of links based on available bandwidth
- Use appropriate load balancing strategies for your use case
- Monitor system resources (CPU, memory, network) during operation

## Examples

See the `examples/` directory for complete usage examples and integration scenarios.

## Contributing

This tool is part of the Aggligator project. See the main project documentation for contribution guidelines.
