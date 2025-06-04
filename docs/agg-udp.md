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

- `--cfg <FILE>`: Configuration file for advanced settings
- `--verbose, -v`: Enable verbose debug logging
- `--stats-interval <SECONDS>`: Statistics reporting interval (default: 5)

### Commands

#### 1. Server Mode

Run a UDP aggregation server:

```bash
agg-udp server [OPTIONS]
```

**Options:**
- `--bind <ADDR>`: Server bind address (default: 0.0.0.0:5801)
- `--target <ADDR>`: Target address to forward aggregated traffic
- `--links <COUNT>`: Number of server links to create (default: 4)
- `--buffer-size <SIZE>`: UDP socket buffer size in bytes (default: 65536)

**Example:**
```bash
# Start server on port 5801, forward to localhost:8080 with 6 links
agg-udp server --bind 0.0.0.0:5801 --target 127.0.0.1:8080 --links 6
```

#### 2. Client Mode

Run a UDP aggregation client:

```bash
agg-udp client [OPTIONS]
```

**Options:**
- `--local <ADDR>`: Local bind address (default: 127.0.0.1:0)
- `--remote <ADDR>`: Remote server addresses (can specify multiple)
- `--strategy <STRATEGY>`: Load balancing strategy
  - `round-robin`: Distribute packets in round-robin fashion
  - `random`: Random link selection
  - `weighted`: Weight-based distribution
  - `fastest`: Use the fastest responding link
- `--timeout <MS>`: Connection timeout in milliseconds (default: 5000)
- `--health-check <MS>`: Health check interval in milliseconds (default: 1000)

**Example:**
```bash
# Connect to multiple servers with round-robin load balancing
agg-udp client --local 127.0.0.1:3000 \
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

Advanced settings can be specified in a TOML configuration file:

```toml
# example-config.toml

[network]
# UDP socket buffer sizes
send_buffer_size = 1048576    # 1MB
recv_buffer_size = 1048576    # 1MB

# Connection settings
connect_timeout_ms = 3000
health_check_interval_ms = 500

[aggregation]
# Load balancing strategy
strategy = "round-robin"  # or "random", "weighted", "fastest"

# Link management
max_links = 16
min_healthy_links = 2

[performance]
# Statistics and monitoring
stats_interval_seconds = 5
enable_detailed_stats = true

# Performance tuning
batch_size = 32
worker_threads = 4

[logging]
level = "info"  # or "debug", "warn", "error"
enable_timestamps = true
```

Use with: `agg-udp --cfg example-config.toml <command>`

## Usage Scenarios

### 1. High-Throughput Data Transfer

**Scenario**: Transfer large amounts of data between two locations with multiple network paths.

**Setup**:
```bash
# On the receiver side
agg-udp server --bind 0.0.0.0:5801 --target 127.0.0.1:8080 --links 8

# On the sender side  
agg-udp client --local 127.0.0.1:3000 \
               --remote server1:5801 \
               --remote server2:5801 \
               --remote server3:5801 \
               --strategy fastest
```

### 2. Load Balancing and Redundancy

**Scenario**: Distribute UDP traffic across multiple servers for load balancing.

**Setup**:
```bash
# Client with weighted load balancing
agg-udp client --remote 192.168.1.10:5801 \
               --remote 192.168.1.11:5801 \
               --remote 192.168.1.12:5801 \
               --strategy weighted
```

### 3. Performance Testing

**Scenario**: Test the capacity of a UDP-based service.

**Setup**:
```bash
# High-rate performance test
agg-udp test --server target-server:5801 \
             --duration 120 \
             --packet-size 1400 \
             --rate 10000 \
             --links 12 \
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
[2024-01-15 10:30:15] UDP Aggregation Stats:
  Total Links: 4 (4 healthy, 0 failed)
  Throughput: 15,432 pps, 23.4 MB/s
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

1. **Unordered Aggregation Core**: Protocol-agnostic packet aggregation
2. **UDP Transport Layer**: UDP-specific networking implementation  
3. **Load Balancing**: Multiple strategies for traffic distribution
4. **Health Monitoring**: Link health detection and failover
5. **Statistics Engine**: Real-time performance monitoring

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
