//! UDP aggregation tool for high-throughput unordered packet aggregation.

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use std::{
    net::SocketAddr,
    path::PathBuf,
    time::Duration,
};
use tokio::{
    net::UdpSocket,
    sync::oneshot,
    time::{sleep, interval},
};
use futures;
use tracing::{info, error, debug};

use aggligator::{
    unordered_cfg::{UnorderedCfg, LoadBalanceStrategy},
    id::LinkId,
};
use aggligator_transport_udp::{UdpTransport, UdpAggregationManager};
use aggligator_util::{init_log, print_default_cfg};

const DEFAULT_UDP_PORT: u16 = 5801;
const DEFAULT_STATS_INTERVAL: u64 = 5; // seconds

/// High-performance UDP packet aggregation tool.
///
/// This tool provides unordered UDP packet aggregation across multiple links
/// for maximum throughput without the overhead of ordering or reliability.
#[derive(Parser)]
#[command(name = "agg-udp", author, version)]
pub struct UdpCli {
    /// Configuration file for advanced settings.
    #[arg(long)]
    cfg: Option<PathBuf>,
    /// Enable verbose debug logging.
    #[arg(long, short = 'v')]
    verbose: bool,
    /// Statistics reporting interval in seconds.
    #[arg(long, default_value_t = DEFAULT_STATS_INTERVAL)]
    stats_interval: u64,
    /// Client or server mode.
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run as UDP aggregation client.
    Client(ClientCli),
    /// Run as UDP aggregation server/relay.
    Server(ServerCli),
    /// Run performance test between client and server.
    Test(TestCli),
    /// Shows the default configuration.
    ShowCfg,
    /// Generate manual pages for this tool in current directory.
    #[command(hide = true)]
    ManPages,
    /// Generate markdown page for this tool.
    #[command(hide = true)]
    Markdown,
}

#[derive(Parser)]
pub struct ClientCli {
    /// Use IPv4 only.
    #[arg(long, short = '4')]
    ipv4: bool,
    /// Use IPv6 only.
    #[arg(long, short = '6')]
    ipv6: bool,
    /// Local address to bind UDP client sockets.
    #[arg(long, short = 'b', default_value = "0.0.0.0:0")]
    bind: SocketAddr,
    /// Remote server addresses and ports.
    ///
    /// Can be specified multiple times to create multiple aggregation links.
    /// Format: ip:port or hostname:port
    #[arg(long, short = 'r', required = true)]
    remote: Vec<String>,
    /// Load balancing strategy.
    ///
    /// round-robin: distribute packets evenly across links.
    /// random: randomly select link for each packet.
    /// weighted: use link performance metrics for selection.
    /// fastest: always use the fastest (lowest latency) link first.
    #[arg(long, default_value = "round-robin")]
    load_balance: String,
    /// Maximum number of packets to send in test mode.
    #[arg(long)]
    max_packets: Option<usize>,
    /// Packet size for test mode (bytes).
    #[arg(long, default_value_t = 1400)]
    packet_size: usize,
    /// Send rate in packets per second (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    rate: u64,
}

#[derive(Parser)]
pub struct ServerCli {
    /// Use IPv4 only.
    #[arg(long, short = '4')]
    ipv4: bool,
    /// Use IPv6 only.
    #[arg(long, short = '6')]
    ipv6: bool,
    /// Local addresses to bind server sockets.
    ///
    /// Can be specified multiple times to listen on multiple interfaces.
    /// Format: ip:port or hostname:port
    #[arg(long, short = 'l', default_value = "0.0.0.0:5801")]
    listen: Vec<String>,
    /// Echo received packets back to sender.
    #[arg(long, short = 'e')]
    echo: bool,
    /// Forward packets to destination address.
    #[arg(long, short = 'f')]
    forward: Option<SocketAddr>,
}

#[derive(Parser)]
pub struct TestCli {
    /// Use IPv4 only.
    #[arg(long, short = '4')]
    ipv4: bool,
    /// Use IPv6 only.
    #[arg(long, short = '6')]
    ipv6: bool,
    /// Local address to bind test client.
    #[arg(long, short = 'b', default_value = "0.0.0.0:0")]
    bind: SocketAddr,
    /// Remote server addresses for performance test.
    #[arg(long, short = 'r', required = true)]
    remote: Vec<String>,
    /// Test duration in seconds.
    #[arg(long, short = 'd', default_value_t = 10)]
    duration: u64,
    /// Packet size for test (bytes).
    #[arg(long, default_value_t = 1400)]
    packet_size: usize,
    /// Send rate in packets per second (0 = unlimited).
    #[arg(long, default_value_t = 1000)]
    rate: u64,
    /// Load balancing strategy for test.
    #[arg(long, default_value = "round-robin")]
    load_balance: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = UdpCli::parse();

    // Initialize logging with appropriate level
    if cli.verbose {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .init();
    } else {
        init_log();
    }

    match cli.command {
        Commands::Client(client) => client.run(cli.stats_interval).await?,
        Commands::Server(server) => server.run(cli.stats_interval).await?,
        Commands::Test(test) => test.run(cli.stats_interval).await?,
        Commands::ShowCfg => print_default_cfg(),
        Commands::ManPages => clap_mangen::generate_to(UdpCli::command(), ".")?,
        Commands::Markdown => println!("{}", clap_markdown::help_markdown::<UdpCli>()),
    }

    Ok(())
}

impl ClientCli {
    async fn run(self, stats_interval: u64) -> Result<()> {
        info!("Starting UDP aggregation client");

        // Parse load balancing strategy
        let load_balance = match self.load_balance.as_str() {
            "round-robin" => LoadBalanceStrategy::RoundRobin,
            "random" => LoadBalanceStrategy::Random,
            "weighted" => LoadBalanceStrategy::WeightedByBandwidth,
            "fastest" => LoadBalanceStrategy::FastestFirst,
            _ => bail!("Invalid load balance strategy: {}", self.load_balance),
        };

        // Create aggregation configuration
        let config = UnorderedCfg {
            load_balance,
            ..UnorderedCfg::default()
        };

        // Create UDP aggregation manager
        let manager = UdpAggregationManager::new(config).await
            .context("Failed to create UDP aggregation manager")?;

        // Parse and connect to remote addresses
        let mut link_id = 1u128;
        for remote_str in &self.remote {
            let remote_addr: SocketAddr = remote_str.parse()
                .with_context(|| format!("Invalid remote address: {}", remote_str))?;

            // Create UDP transport
            let transport = UdpTransport::connect(self.bind, remote_addr).await
                .with_context(|| format!("Failed to connect to {}", remote_addr))?;

            info!("Connected UDP link {} -> {}", transport.local_addr(), remote_addr);

            // Add to aggregation
            manager.add_udp_link(LinkId(link_id), transport).await
                .context("Failed to add UDP link to aggregation")?;

            link_id += 1;
        }

        // Start statistics reporting
        let stats_manager = manager.control_sender();
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(stats_interval));
            loop {
                interval.tick().await;
                
                let (tx, rx) = oneshot::channel();
                if stats_manager.send(aggligator::unordered_task::UnorderedControlMsg::GetStats { respond_to: tx }).is_ok() {
                    if let Ok(stats) = rx.await {
                        info!(
                            "UDP Aggregation Stats: {} active links, {} healthy, {} bytes sent, {} bytes received",
                            stats.active_links, stats.healthy_links, stats.total_bytes_sent, stats.total_bytes_received
                        );
                        
                        for (link_id, link_stats) in &stats.link_stats {
                            debug!(
                                "Link {}: {} -> healthy={}, sent={}, received={}, success_rate={:.2}%",
                                link_id,
                                link_stats.remote_addr,
                                link_stats.is_healthy,
                                link_stats.bytes_sent,
                                link_stats.bytes_received,
                                if link_stats.successful_sends > 0 {
                                    100.0 * link_stats.successful_sends as f64 / 
                                    (link_stats.successful_sends + link_stats.failed_sends) as f64
                                } else { 0.0 }
                            );
                        }
                    }
                }
            }
        });

        // Run client logic
        if let Some(max_packets) = self.max_packets {
            // Test mode with limited packets
            info!("Sending {} test packets", max_packets);
            for i in 0..max_packets {
                let data = format!("Test packet {} - {}", i, "x".repeat(self.packet_size.saturating_sub(50))).into_bytes();
                
                match manager.send_data(data).await {
                    Ok(bytes_sent) => {
                        debug!("Sent packet {}: {} bytes", i, bytes_sent);
                    }
                    Err(e) => {
                        error!("Failed to send packet {}: {}", i, e);
                    }
                }

                if self.rate > 0 {
                    let delay = Duration::from_secs_f64(1.0 / self.rate as f64);
                    sleep(delay).await;
                }
            }
            info!("Completed sending {} packets", max_packets);
        } else {
            // Interactive mode
            info!("UDP aggregation client running. Press Ctrl+C to exit.");
            
            // For now, just keep running and report stats
            let mut interval = interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                let stats = manager.get_stats().await?;
                info!("Client running with {} active links", stats.active_links);
            }
        }

        Ok(())
    }
}

impl ServerCli {
    async fn run(self, stats_interval: u64) -> Result<()> {
        info!("Starting UDP aggregation server");

        let mut servers = Vec::new();

        // Bind to all specified listen addresses
        for listen_str in &self.listen {
            let listen_addr: SocketAddr = listen_str.parse()
                .with_context(|| format!("Invalid listen address: {}", listen_str))?;

            let socket = UdpSocket::bind(listen_addr).await
                .with_context(|| format!("Failed to bind to {}", listen_addr))?;

            let actual_addr = socket.local_addr()?;
            info!("UDP server listening on {}", actual_addr);

            servers.push((socket, actual_addr));
        }

        // Start server tasks
        let mut server_tasks = Vec::new();
        let server_count = servers.len();
        
        for (socket, addr) in servers {
            let echo = self.echo;
            let forward = self.forward;
            
            let task = tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                
                loop {
                    match socket.recv_from(&mut buf).await {
                        Ok((len, from)) => {
                            let data = &buf[..len];
                            debug!("Received {} bytes from {}", len, from);

                            if echo {
                                // Echo back to sender
                                if let Err(e) = socket.send_to(data, from).await {
                                    error!("Failed to echo to {}: {}", from, e);
                                } else {
                                    debug!("Echoed {} bytes back to {}", len, from);
                                }
                            }

                            if let Some(forward_addr) = forward {
                                // Forward to destination
                                if let Err(e) = socket.send_to(data, forward_addr).await {
                                    error!("Failed to forward to {}: {}", forward_addr, e);
                                } else {
                                    debug!("Forwarded {} bytes to {}", len, forward_addr);
                                }
                            }
                        }
                        Err(e) => {
                            error!("Server {} receive error: {}", addr, e);
                        }
                    }
                }
            });
            
            server_tasks.push(task);
        }

        // Statistics reporting
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(stats_interval));
            loop {
                interval.tick().await;
                info!("UDP server running with {} listeners", server_count);
            }
        });

        // Wait for all server tasks
        futures::future::try_join_all(server_tasks).await?;

        Ok(())
    }
}

impl TestCli {
    async fn run(self, stats_interval: u64) -> Result<()> {
        info!("Starting UDP aggregation performance test");

        // Parse load balancing strategy
        let load_balance = match self.load_balance.as_str() {
            "round-robin" => LoadBalanceStrategy::RoundRobin,
            "random" => LoadBalanceStrategy::Random,
            "weighted" => LoadBalanceStrategy::WeightedByBandwidth,
            "fastest" => LoadBalanceStrategy::FastestFirst,
            _ => bail!("Invalid load balance strategy: {}", self.load_balance),
        };

        // Create aggregation configuration
        let config = UnorderedCfg {
            load_balance,
            ..UnorderedCfg::default()
        };

        // Create UDP aggregation manager
        let manager = UdpAggregationManager::new(config).await
            .context("Failed to create UDP aggregation manager")?;

        // Connect to test servers
        let mut link_id = 1u128;
        for remote_str in &self.remote {
            let remote_addr: SocketAddr = remote_str.parse()
                .with_context(|| format!("Invalid remote address: {}", remote_str))?;

            let transport = UdpTransport::connect(self.bind, remote_addr).await
                .with_context(|| format!("Failed to connect to {}", remote_addr))?;

            info!("Test link {} -> {}", transport.local_addr(), remote_addr);

            manager.add_udp_link(LinkId(link_id), transport).await
                .context("Failed to add UDP link to aggregation")?;

            link_id += 1;
        }

        // Performance test
        let test_data = vec![42u8; self.packet_size];
        let test_duration = Duration::from_secs(self.duration);
        let packet_interval = if self.rate > 0 {
            Some(Duration::from_secs_f64(1.0 / self.rate as f64))
        } else {
            None
        };

        info!("Running performance test for {} seconds", self.duration);
        info!("Packet size: {} bytes, Rate: {} pps", self.packet_size, if self.rate > 0 { self.rate.to_string() } else { "unlimited".to_string() });

        let start_time = std::time::Instant::now();
        let mut packets_sent = 0u64;
        let mut bytes_sent = 0u64;

        while start_time.elapsed() < test_duration {
            match manager.send_data(test_data.clone()).await {
                Ok(sent) => {
                    packets_sent += 1;
                    bytes_sent += sent as u64;
                }
                Err(e) => {
                    error!("Send error: {}", e);
                }
            }

            if let Some(interval) = packet_interval {
                sleep(interval).await;
            }
        }

        let elapsed = start_time.elapsed();
        let final_stats = manager.get_stats().await?;

        // Print test results
        info!("=== Performance Test Results ===");
        info!("Test Duration: {:.2}s", elapsed.as_secs_f64());
        info!("Packets Sent: {}", packets_sent);
        info!("Bytes Sent: {} ({:.2} MB)", bytes_sent, bytes_sent as f64 / 1_000_000.0);
        info!("Packet Rate: {:.2} pps", packets_sent as f64 / elapsed.as_secs_f64());
        info!("Throughput: {:.2} Mbps", (bytes_sent * 8) as f64 / elapsed.as_secs_f64() / 1_000_000.0);
        info!("Active Links: {}", final_stats.active_links);
        info!("Healthy Links: {}", final_stats.healthy_links);

        for (link_id, link_stats) in &final_stats.link_stats {
            info!(
                "Link {}: sent={} bytes, success_rate={:.2}%",
                link_id,
                link_stats.bytes_sent,
                if link_stats.successful_sends > 0 {
                    100.0 * link_stats.successful_sends as f64 / 
                    (link_stats.successful_sends + link_stats.failed_sends) as f64
                } else { 0.0 }
            );
        }

        Ok(())
    }
}
