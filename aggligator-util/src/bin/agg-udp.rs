//! UDP aggregation tool for high-throughput unordered packet aggregation.

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use crossterm::{style::Stylize, tty::IsTty};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::stdout,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, atomic::{AtomicU64, Ordering}},
    time::Duration,
};
use tokio::{
    net::UdpSocket,
    sync::{oneshot, RwLock},
    time::{sleep, interval},
};
use futures;
use tracing::{info, error, debug, warn};

use aggligator::{
    unordered_cfg::{UnorderedCfg, LoadBalanceStrategy},
    id::LinkId,
};
use aggligator_transport_udp::{UdpTransport, UdpAggregationManager};
use aggligator_util::{init_log, session::{SessionManager, SessionPacket, SessionPacketBuilder}};

const DEFAULT_STATS_INTERVAL: u64 = 5; // seconds

/// Configuration structure for UDP aggregation that can be loaded from JSON file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpAggConfig {
    /// Load balancing strategy
    pub load_balance: String,
    /// Send queue size (default: 256)
    pub send_queue: Option<usize>,
    /// Receive queue size (default: 256)
    pub recv_queue: Option<usize>,
    /// Heartbeat interval in milliseconds (default: 100)
    pub heartbeat_interval_ms: Option<u64>,
    /// Link timeout in milliseconds (default: 500)
    pub link_timeout_ms: Option<u64>,
    /// Maximum packet size in bytes (default: 1472)
    pub max_packet_size: Option<usize>,
    /// Connection queue size (default: 16)
    pub connect_queue: Option<usize>,
}

impl Default for UdpAggConfig {
    fn default() -> Self {
        Self {
            load_balance: "packet-round-robin".to_string(), // Use packet-round-robin for true bandwidth aggregation
            send_queue: None,
            recv_queue: None,
            heartbeat_interval_ms: None,
            link_timeout_ms: None,
            max_packet_size: None,
            connect_queue: None,
        }
    }
}

impl UdpAggConfig {
    /// Convert to UnorderedCfg with validation
    pub fn to_unordered_cfg(&self) -> Result<UnorderedCfg> {
        let load_balance = match self.load_balance.as_str() {
            "round-robin" => LoadBalanceStrategy::RoundRobin,
            "packet-round-robin" => LoadBalanceStrategy::PacketRoundRobin,
            "random" => LoadBalanceStrategy::Random,
            "weighted" => LoadBalanceStrategy::WeightedByBandwidth,
            "fastest" => LoadBalanceStrategy::FastestFirst,
            _ => bail!("Invalid load balance strategy: {}. Valid options: round-robin, packet-round-robin, random, weighted, fastest", self.load_balance),
        };

        let mut config = UnorderedCfg {
            load_balance,
            ..UnorderedCfg::default()
        };

        if let Some(send_queue) = self.send_queue {
            config.send_queue = std::num::NonZeroUsize::new(send_queue)
                .context("send_queue must be greater than 0")?;
        }

        if let Some(recv_queue) = self.recv_queue {
            config.recv_queue = std::num::NonZeroUsize::new(recv_queue)
                .context("recv_queue must be greater than 0")?;
        }

        if let Some(ms) = self.heartbeat_interval_ms {
            config.heartbeat_interval = Duration::from_millis(ms);
        }

        if let Some(ms) = self.link_timeout_ms {
            config.link_timeout = Duration::from_millis(ms);
        }

        if let Some(size) = self.max_packet_size {
            config.max_packet_size = size;
        }

        if let Some(connect_queue) = self.connect_queue {
            config.connect_queue = std::num::NonZeroUsize::new(connect_queue)
                .context("connect_queue must be greater than 0")?;
        }

        if let Err(e) = config.validate() {
            bail!("Invalid configuration: {}", e);
        }
        Ok(config)
    }
}

/// Load UDP aggregation configuration from file or use default
pub fn load_udp_config(path: &Option<PathBuf>) -> Result<UnorderedCfg> {
    match path {
        Some(path) => {
            let file = std::fs::File::open(path)
                .with_context(|| format!("Cannot open configuration file: {}", path.display()))?;
            let config: UdpAggConfig = serde_json::from_reader(file)
                .with_context(|| format!("Cannot parse configuration file: {}", path.display()))?;
            config.to_unordered_cfg()
        }
        None => Ok(UnorderedCfg::default()),
    }
}

/// Print the default UDP configuration
pub fn print_default_udp_cfg() {
    let default_config = UdpAggConfig::default();
    let json = serde_json::to_string_pretty(&default_config).unwrap();
    println!("{}", json);
}

/// Connection statistics for monitoring
#[derive(Default)]
pub struct ConnectionStats {
    pub packets_sent: AtomicU64,
    pub packets_received: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub connections_established: AtomicU64,
    pub connection_errors: AtomicU64,
}

impl ConnectionStats {
    pub fn new() -> Self {
        Self::default()
    }
    
    pub fn record_packet_sent(&self, bytes: usize) {
        self.packets_sent.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent.fetch_add(bytes as u64, Ordering::Relaxed);
    }
    
    pub fn record_packet_received(&self, bytes: usize) {
        self.packets_received.fetch_add(1, Ordering::Relaxed);
        self.bytes_received.fetch_add(bytes as u64, Ordering::Relaxed);
    }
    
    pub fn record_connection_established(&self) {
        self.connections_established.fetch_add(1, Ordering::Relaxed);
    }
    
    pub fn record_connection_error(&self) {
        self.connection_errors.fetch_add(1, Ordering::Relaxed);
    }
    
    pub fn get_summary(&self) -> (u64, u64, u64, u64, u64, u64) {
        (
            self.packets_sent.load(Ordering::Relaxed),
            self.packets_received.load(Ordering::Relaxed),
            self.bytes_sent.load(Ordering::Relaxed),
            self.bytes_received.load(Ordering::Relaxed),
            self.connections_established.load(Ordering::Relaxed),
            self.connection_errors.load(Ordering::Relaxed),
        )
    }
}

/// Print connection status with styling
pub fn print_status(message: &str, status: &str, use_color: bool) {
    if use_color {
        println!("{} {}", "🔗".blue(), format!("{}: {}", message, status).bold());
    } else {
        println!("🔗 {}: {}", message, status);
    }
}

/// Print error with styling
pub fn print_error(message: &str, error: &str, use_color: bool) {
    if use_color {
        println!("{} {}", "❌".red(), format!("{}: {}", message, error).red());
    } else {
        println!("❌ {}: {}", message, error);
    }
}

/// Print success with styling
pub fn print_success(message: &str, use_color: bool) {
    if use_color {
        println!("{} {}", "✅".green(), message.green());
    } else {
        println!("✅ {}", message);
    }
}

/// Print statistics summary
pub fn print_stats(stats: &ConnectionStats, use_color: bool) {
    let (packets_sent, packets_received, bytes_sent, bytes_received, connections, errors) = stats.get_summary();
    
    let stats_msg = format!(
        "📊 Stats: {} pkts sent ({:.2} MB), {} pkts recv ({:.2} MB), {} conns, {} errors",
        packets_sent,
        bytes_sent as f64 / 1024.0 / 1024.0,
        packets_received,
        bytes_received as f64 / 1024.0 / 1024.0,
        connections,
        errors
    );
    
    if use_color {
        println!("{}", stats_msg.cyan());
    } else {
        println!("{}", stats_msg);
    }
}

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
    /// Local proxy port to listen for client connections (transparent proxy mode).
    #[arg(long)]
    proxy_port: Option<u16>,
    /// Target server address for transparent proxy mode.
    #[arg(long)]
    target: Option<SocketAddr>,
    /// Session timeout in seconds for transparent proxy.
    #[arg(long, default_value_t = 60)]
    session_timeout: u64,
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
    /// Session timeout in seconds for transparent proxy.
    #[arg(long, default_value_t = 60)]
    session_timeout: u64,
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
        Commands::Client(client) => client.run(cli.stats_interval, &cli.cfg).await?,
        Commands::Server(server) => server.run(cli.stats_interval, &cli.cfg).await?,
        Commands::Test(test) => test.run(cli.stats_interval, &cli.cfg).await?,
        Commands::ShowCfg => print_default_udp_cfg(),
        Commands::ManPages => clap_mangen::generate_to(UdpCli::command(), ".")?,
        Commands::Markdown => println!("{}", clap_markdown::help_markdown::<UdpCli>()),
    }

    Ok(())
}

impl ClientCli {
    async fn run(self, stats_interval: u64, cfg_path: &Option<PathBuf>) -> Result<()> {
        let use_color = stdout().is_tty();
        let stats = Arc::new(ConnectionStats::new());
        
        print_status("UDP Aggregation Client", "Starting up", use_color);

        // Load configuration from file or use defaults with CLI overrides
        let mut config = load_udp_config(cfg_path)?;
        
        // Override load balancing strategy if specified via CLI
        let load_balance = match self.load_balance.as_str() {
            "round-robin" => LoadBalanceStrategy::RoundRobin,
            "random" => LoadBalanceStrategy::Random,
            "weighted" => LoadBalanceStrategy::WeightedByBandwidth,
            "fastest" => LoadBalanceStrategy::FastestFirst,
            _ => {
                print_error("Configuration", &format!("Invalid load balance strategy: {}", self.load_balance), use_color);
                bail!("Invalid load balance strategy: {}", self.load_balance);
            }
        };
        config.load_balance = load_balance;

        print_status("Load Balancing", &self.load_balance, use_color);
        
        if cfg_path.is_some() {
            print_status("Configuration", "Loaded from file", use_color);
        }

        // Create UDP aggregation manager
        print_status("Aggregation Manager", "Initializing", use_color);
        let manager = UdpAggregationManager::new(config).await
            .context("Failed to create UDP aggregation manager")?;
        print_success("Aggregation Manager ready", use_color);

        // Parse and connect to remote addresses
        let mut link_id = 1u128;
        let total_remotes = self.remote.len();
        
        if use_color {
            println!("{} Connecting to {} remote server(s):", "🔗".blue(), total_remotes.to_string().bold());
        } else {
            println!("🔗 Connecting to {} remote server(s):", total_remotes);
        }
        
        for (idx, remote_str) in self.remote.iter().enumerate() {
            let remote_addr: SocketAddr = remote_str.parse()
                .with_context(|| format!("Invalid remote address: {}", remote_str))?;

            if use_color {
                println!("  [{}/{}] 📡 Connecting to {}...", (idx + 1).to_string().cyan(), total_remotes.to_string().cyan(), remote_addr.to_string().yellow());
            } else {
                println!("  [{}/{}] 📡 Connecting to {}...", idx + 1, total_remotes, remote_addr);
            }

            // Create UDP transport
            match UdpTransport::connect(self.bind, remote_addr).await {
                Ok(transport) => {
                    let local_addr = transport.local_addr();
                    if use_color {
                        println!("  [{}/{}] ✅ Connected {} -> {} (local: {})", 
                                (idx + 1).to_string().cyan(), 
                                total_remotes.to_string().cyan(), 
                                link_id.to_string().green(), 
                                remote_addr.to_string().yellow(),
                                local_addr.to_string().dim());
                    } else {
                        println!("  [{}/{}] ✅ Connected {} -> {} (local: {})", 
                                idx + 1, total_remotes, link_id, remote_addr, local_addr);
                    }

                    // Add to aggregation
                    match manager.add_udp_link(LinkId(link_id), transport).await {
                        Ok(_) => {
                            if use_color {
                                println!("  [{}/{}] 🚀 Added link {} to aggregation", 
                                        (idx + 1).to_string().cyan(), 
                                        total_remotes.to_string().cyan(), 
                                        link_id.to_string().green());
                            } else {
                                println!("  [{}/{}] 🚀 Added link {} to aggregation", idx + 1, total_remotes, link_id);
                            }
                            stats.record_connection_established();
                        }
                        Err(e) => {
                            stats.record_connection_error();
                            print_error(&format!("Link {}", link_id), &format!("Failed to add to aggregation: {}", e), use_color);
                            return Err(e.into());
                        }
                    }
                }
                Err(e) => {
                    stats.record_connection_error();
                    print_error(&format!("Connection to {}", remote_addr), &e.to_string(), use_color);
                    return Err(e.into());
                }
            }

            link_id += 1;
        }

        if use_color {
            println!("{} All {} links connected successfully!", "🌐".green().bold(), total_remotes.to_string().bold().green());
        } else {
            println!("🌐 All {} links connected successfully!", total_remotes);
        }

        // Start statistics reporting
        let stats_clone = stats.clone();
        let manager_stats = manager.control_sender();
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(stats_interval));
            loop {
                interval.tick().await;
                
                print_stats(&stats_clone, use_color);
                
                let (tx, rx) = oneshot::channel();
                if manager_stats.send(aggligator::unordered_task::UnorderedControlMsg::GetStats { respond_to: tx }).is_ok() {
                    if let Ok(link_stats) = rx.await {
                        if use_color {
                            println!("{} Aggregation: {} active links, {} healthy, {:.2} MB sent, {:.2} MB received",
                                    "📊".cyan(),
                                    link_stats.active_links.to_string().bold(),
                                    link_stats.healthy_links.to_string().green(),
                                    link_stats.total_bytes_sent as f64 / 1024.0 / 1024.0,
                                    link_stats.total_bytes_received as f64 / 1024.0 / 1024.0
                            );
                        } else {
                            println!("📊 Aggregation: {} active links, {} healthy, {:.2} MB sent, {:.2} MB received",
                                    link_stats.active_links, link_stats.healthy_links,
                                    link_stats.total_bytes_sent as f64 / 1024.0 / 1024.0,
                                    link_stats.total_bytes_received as f64 / 1024.0 / 1024.0
                            );
                        }
                        
                        for (link_id, link_stat) in &link_stats.link_stats {
                            let success_rate = if link_stat.successful_sends > 0 {
                                100.0 * link_stat.successful_sends as f64 / 
                                (link_stat.successful_sends + link_stat.failed_sends) as f64
                            } else { 0.0 };
                            
                            let health_icon = if link_stat.is_healthy { "✅" } else { "❌" };
                            
                            if use_color {
                                println!("    {} Link {}: {} (success: {:.1}%, sent: {:.2} MB, recv: {:.2} MB)",
                                        health_icon,
                                        link_id.to_string().cyan(),
                                        link_stat.remote_addr.to_string().yellow(),
                                        success_rate,
                                        link_stat.bytes_sent as f64 / 1024.0 / 1024.0,
                                        link_stat.bytes_received as f64 / 1024.0 / 1024.0
                                );
                            } else {
                                println!("    {} Link {}: {} (success: {:.1}%, sent: {:.2} MB, recv: {:.2} MB)",
                                        health_icon, link_id, link_stat.remote_addr, success_rate,
                                        link_stat.bytes_sent as f64 / 1024.0 / 1024.0,
                                        link_stat.bytes_received as f64 / 1024.0 / 1024.0
                                );
                            }
                        }
                    }
                }
            }
        });

        // Check if transparent proxy mode is enabled
        if let (Some(proxy_port), Some(target)) = (self.proxy_port, self.target) {
            let title = if use_color {
                format!("{} Transparent Proxy: :{} -> {}", "🔀".blue(), proxy_port.to_string().cyan(), target.to_string().yellow())
            } else {
                format!("🔀 Transparent Proxy: :{} -> {}", proxy_port, target)
            };
            println!("{}", title);
            
            // Transparent proxy mode
            self.run_transparent_proxy(manager, proxy_port, target, stats).await
        } else {
            print_status("Mode", "Test/Interactive", use_color);
            // Test/interactive mode
            self.run_test_mode(manager, stats).await
        }
    }

    async fn run_transparent_proxy(
        &self,
        manager: UdpAggregationManager,
        proxy_port: u16,
        target: SocketAddr,
        stats: Arc<ConnectionStats>,
    ) -> Result<()> {
        let use_color = stdout().is_tty();
        
        if use_color {
            println!("{} Starting transparent proxy on {} -> {}", 
                    "🌐".green(), 
                    format!(":{}", proxy_port).cyan(), 
                    target.to_string().yellow());
        } else {
            println!("🌐 Starting transparent proxy on :{} -> {}", proxy_port, target);
        }

        // Create session manager for client tracking
        let session_manager = Arc::new(SessionManager::new(Duration::from_secs(self.session_timeout)));

        // Bind proxy socket to listen for clients
        let proxy_addr = SocketAddr::new("0.0.0.0".parse().unwrap(), proxy_port);
        let proxy_socket = Arc::new(UdpSocket::bind(proxy_addr).await
            .with_context(|| format!("Failed to bind proxy socket to {}", proxy_addr))?);

        let local_addr = proxy_socket.local_addr()?;
        if use_color {
            println!("{} Proxy listening on {}", "👂".blue(), local_addr.to_string().green());
            println!("{} Target service: {}", "🎯".blue(), target.to_string().yellow());
        } else {
            println!("👂 Proxy listening on {}", local_addr);
            println!("🎯 Target service: {}", target);
        }

        // Clone for tasks
        let proxy_socket_recv = proxy_socket.clone();
        let session_manager_recv = session_manager.clone();

        // Wrap manager in Arc for sharing
        let manager = Arc::new(manager);

        // Task 1: Forward client -> target (via aggregation)
        let manager_send = manager.clone();
        let stats_send = stats.clone();
        let client_to_target = tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            if use_color {
                println!("{} Started client->target forwarding task", "📥".cyan());
            } else {
                println!("📥 Started client->target forwarding task");
            }
            
            loop {
                match proxy_socket_recv.recv_from(&mut buf).await {
                    Ok((len, client_addr)) => {
                        let client_data = &buf[..len];
                        debug!("📦 Received {} bytes from client {}", len, client_addr);
                        stats_send.record_packet_received(len);

                        // Generate UDP session ID based on the 5-tuple for consistent routing
                        let session_id = SessionManager::generate_udp_session_id(
                            client_addr,
                            target,  // The target server address
                            17  // UDP protocol number
                        );
                        debug!("🔑 Generated UDP session ID {} for flow {} -> {}", session_id, client_addr, target);

                        // Store mapping for session tracking (for reverse path)
                        session_manager_recv.get_or_create_session(client_addr);

                        // Send directly via aggregation with session affinity (NO session packet wrapper needed)
                        // The aggregation layer will automatically wrap data in UnorderedLinkMsg::Data
                        match manager_send.send_data_with_session(client_data.to_vec(), session_id, Some(client_addr)).await {
                            Ok(send_result) => {
                                debug!("✅ Forwarded {} bytes (session {}) to target via aggregation link {}", 
                                       send_result.bytes_sent, session_id, send_result.link_id);
                                stats_send.record_packet_sent(send_result.bytes_sent);
                            }
                            Err(e) => {
                                error!("❌ Failed to forward client data via aggregation: {}", e);
                                stats_send.record_connection_error();
                            }
                        }
                    }
                    Err(e) => {
                        error!("❌ Proxy receive error: {}", e);
                        stats_send.record_connection_error();
                    }
                }
            }
        });

        // Task 2: Forward target -> client (from aggregation)
        let manager_recv = manager.clone();
        let proxy_socket_send = proxy_socket.clone();
        let session_manager_send = session_manager.clone();
        let stats_recv = stats.clone();
        
        let target_to_client = tokio::spawn(async move {
            if use_color {
                println!("{} Started target->client forwarding task", "📤".cyan());
            } else {
                println!("📤 Started target->client forwarding task");
            }
            
            // Get receiver for data coming back from aggregation
            if let Some(mut receiver) = manager_recv.get_receiver().await {
                if use_color {
                    println!("{} Connected to aggregation receiver", "🔄".green());
                } else {
                    println!("🔄 Connected to aggregation receiver");
                }
                
                while let Some(received_data) = receiver.recv().await {
                    debug!("📨 Received {} bytes from aggregation on link {} (session: {})", 
                           received_data.data.len(), received_data.link_id, received_data.session_id);
                    stats_recv.record_packet_received(received_data.data.len());
                    
                    // Check if we have session information from UnorderedLinkMsg::Data
                    if received_data.session_id != 0 && received_data.original_client.is_some() {
                        // This is properly encapsulated multi-link data
                        let client_addr = received_data.original_client.unwrap();
                        let payload = &received_data.data;
                        
                        debug!("🎯 Forwarding {} bytes back to original client {} (session {})", 
                               payload.len(), client_addr, received_data.session_id);
                        
                        // Send payload back to original client
                        match proxy_socket_send.send_to(payload, client_addr).await {
                            Ok(bytes_sent) => {
                                debug!("✅ Successfully sent {} bytes back to client {} via multi-link aggregation", 
                                       bytes_sent, client_addr);
                                stats_recv.record_packet_sent(bytes_sent);
                            }
                            Err(e) => {
                                error!("❌ Failed to send response back to client {}: {}", client_addr, e);
                                stats_recv.record_connection_error();
                            }
                        }
                    } else {
                        // Fallback: try to parse as legacy session packet for backward compatibility
                        if let Some(session_packet) = SessionPacket::new(&received_data.data) {
                            if let Some(header) = session_packet.header() {
                                let session_id = header.session_id;
                                let payload = session_packet.payload();
                                
                                debug!("🔍 Parsed legacy session packet: session={}, payload={} bytes", session_id, payload.len());
                                
                                // Look up client address for this session
                                if let Some(client_addr) = session_manager_send.get_client_addr(session_id) {
                                    debug!("🎯 Forwarding {} bytes back to client {} (legacy session {})", 
                                           payload.len(), client_addr, session_id);
                                    
                                    // Send payload back to client
                                    match proxy_socket_send.send_to(payload, client_addr).await {
                                        Ok(bytes_sent) => {
                                            debug!("✅ Successfully sent {} bytes back to client {} (legacy mode)", 
                                                   bytes_sent, client_addr);
                                            stats_recv.record_packet_sent(bytes_sent);
                                        }
                                        Err(e) => {
                                            error!("❌ Failed to send response back to client {}: {}", client_addr, e);
                                            stats_recv.record_connection_error();
                                        }
                                    }
                                } else {
                                    warn!("⚠️  No client address found for legacy session {}", session_id);
                                }
                            } else {
                                warn!("⚠️  Received data from aggregation without session header");
                            }
                        } else {
                            warn!("⚠️  Received malformed session packet from aggregation");
                        }
                    }
                }
            } else {
                print_error("Aggregation Receiver", "Failed to connect", use_color);
                return;
            }
        });

        // Task 3: Session cleanup
        let session_cleanup = tokio::spawn(async move {
            let mut cleanup_interval = interval(Duration::from_secs(30));
            loop {
                cleanup_interval.tick().await;
                let expired = session_manager.cleanup_expired_sessions();
                if expired > 0 {
                    if use_color {
                        println!("{} Cleaned up {} expired sessions", "🧹".dim(), expired.to_string().dim());
                    } else {
                        println!("🧹 Cleaned up {} expired sessions", expired);
                    }
                }
            }
        });

        // Wait for tasks
        tokio::select! {
            _ = client_to_target => {
                print_error("Proxy", "Client-to-target task terminated", use_color);
            }
            _ = target_to_client => {
                print_error("Proxy", "Target-to-client task terminated", use_color);
            }
            _ = session_cleanup => {
                print_error("Proxy", "Session cleanup task terminated", use_color);
            }
        }

        Ok(())
    }

    async fn run_test_mode(&self, manager: UdpAggregationManager, stats: Arc<ConnectionStats>) -> Result<()> {
        let use_color = stdout().is_tty();
        
        // Run client logic
        if let Some(max_packets) = self.max_packets {
            // Test mode with limited packets
            print_status("Test Mode", &format!("Sending {} packets", max_packets), use_color);
            
            for i in 0..max_packets {
                let data = format!("Test packet {} - {}", i, "x".repeat(self.packet_size.saturating_sub(50))).into_bytes();
                
                match manager.send_data(data.clone()).await {
                    Ok(bytes_sent) => {
                        debug!("Sent packet {}: {} bytes", i, bytes_sent);
                        stats.record_packet_sent(bytes_sent);
                    }
                    Err(e) => {
                        error!("Failed to send packet {}: {}", i, e);
                        stats.record_connection_error();
                    }
                }

                if self.rate > 0 {
                    let delay = Duration::from_secs_f64(1.0 / self.rate as f64);
                    sleep(delay).await;
                }
            }
            print_success(&format!("Completed sending {} packets", max_packets), use_color);
        } else {
            // Interactive mode
            print_status("Interactive Mode", "Client running (Press Ctrl+C to exit)", use_color);
            
            // For now, just keep running and report stats
            let mut interval = interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                let stats_result = manager.get_stats().await?;
                
                if use_color {
                    println!("{} Client running with {} active links", "🏃".cyan(), stats_result.active_links.to_string().bold());
                } else {
                    println!("🏃 Client running with {} active links", stats_result.active_links);
                }
            }
        }

        Ok(())
    }
}

impl ServerCli {
    async fn run(self, stats_interval: u64, cfg_path: &Option<PathBuf>) -> Result<()> {
        let use_color = stdout().is_tty();
        let stats = Arc::new(ConnectionStats::new());
        
        print_status("UDP Aggregation Server", "Starting up", use_color);

        // Load configuration from file if provided
        let config = load_udp_config(cfg_path)?;
        if cfg_path.is_some() {
            print_status("Configuration", "Loaded from file", use_color);
        }

        // Create session manager for transparent proxy
        let session_manager = Arc::new(SessionManager::new(Duration::from_secs(self.session_timeout)));
        print_success("Session manager ready", use_color);

        let mut servers = Vec::new();
        let mut server_tasks = Vec::new();

        // Bind to all specified listen addresses
        if use_color {
            println!("{} Binding to {} listen address(es):", "🔗".blue(), self.listen.len().to_string().bold());
        } else {
            println!("🔗 Binding to {} listen address(es):", self.listen.len());
        }
        
        for (idx, listen_str) in self.listen.iter().enumerate() {
            let listen_addr: SocketAddr = listen_str.parse()
                .with_context(|| format!("Invalid listen address: {}", listen_str))?;

            if use_color {
                println!("  [{}/{}] 📡 Binding to {}...", (idx + 1).to_string().cyan(), self.listen.len().to_string().cyan(), listen_addr.to_string().yellow());
            } else {
                println!("  [{}/{}] 📡 Binding to {}...", idx + 1, self.listen.len(), listen_addr);
            }

            match UdpSocket::bind(listen_addr).await {
                Ok(socket) => {
                    let actual_addr = socket.local_addr()?;
                    if use_color {
                        println!("  [{}/{}] ✅ Server listening on {}", (idx + 1).to_string().cyan(), self.listen.len().to_string().cyan(), actual_addr.to_string().green());
                    } else {
                        println!("  [{}/{}] ✅ Server listening on {}", idx + 1, self.listen.len(), actual_addr);
                    }
                    servers.push((socket, actual_addr));
                }
                Err(e) => {
                    print_error(&format!("Bind to {}", listen_addr), &e.to_string(), use_color);
                    return Err(e.into());
                }
            }
        }

        // Print server mode information
        if self.echo {
            print_status("Mode", "Echo mode enabled", use_color);
        }
        if let Some(forward_addr) = self.forward {
            if use_color {
                println!("{} Forward mode: {} -> {}", "🔀".blue(), "clients".cyan(), forward_addr.to_string().yellow());
            } else {
                println!("🔀 Forward mode: clients -> {}", forward_addr);
            }
        }

        // Note: Server doesn't need its own aggregation manager for responses.
        // Responses are sent back through the same UDP socket that received the request.

        // Create a socket for communicating with the target service if in forward mode
        let target_socket = if let Some(forward_addr) = self.forward {
            match UdpSocket::bind("0.0.0.0:0").await {
                Ok(target_socket) => {
                    let local_addr = target_socket.local_addr()?;
                    if use_color {
                        println!("{} Target socket created: {} -> {}", "🎯".blue(), local_addr.to_string().dim(), forward_addr.to_string().yellow());
                    } else {
                        println!("🎯 Target socket created: {} -> {}", local_addr, forward_addr);
                    }
                    Some(Arc::new(target_socket))
                }
                Err(e) => {
                    print_error("Target socket", &e.to_string(), use_color);
                    return Err(e.into());
                }
            }
        } else {
            None
        };

        // Start server tasks for each listen address
        for (idx, (socket, addr)) in servers.into_iter().enumerate() {
            let echo = self.echo;
            let forward = self.forward;
            let session_manager = session_manager.clone();
            let target_socket = target_socket.clone();
            let stats_task = stats.clone();
            let socket = Arc::new(socket);
            
            if use_color {
                println!("{} Started server task {} for {}", "🚀".green(), (idx + 1).to_string().cyan(), addr.to_string().yellow());
            } else {
                println!("🚀 Started server task {} for {}", idx + 1, addr);
            }
            
            let task = tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                
                loop {
                    match socket.recv_from(&mut buf).await {
                        Ok((len, from)) => {
                            let data = &buf[..len];
                            debug!("📦 Received {} bytes from {}", len, from);
                            stats_task.record_packet_received(len);

                            // Try to parse session packet
                            if let Some(session_packet) = SessionPacket::new(data) {
                                if let Some(header) = session_packet.header() {
                                    // This is a session-tracked packet from aggregation client
                                    let payload = session_packet.payload();
                                    let session_id = header.session_id;
                                    
                                    debug!("🔍 Processing session packet: session_id={}, payload_len={}", 
                                           session_id, payload.len());

                                    // The session header contains CRC32 hash and port of the original client.
                                    // We need to reconstruct the original client address for proper response routing.
                                    // For now, we'll create a placeholder address and rely on session_id mapping.
                                    let original_client_addr = SocketAddr::new(
                                        "127.0.0.1".parse().unwrap(), 
                                        header.client_port
                                    );

                                    // Remember complete session information for response routing
                                    session_manager.remember_session_for_server(session_id, original_client_addr, from);
                                    debug!("🔑 Stored session {} mapping: {} -> {} (via agg client {})", 
                                           session_id, original_client_addr, addr, from);

                                    if echo {
                                        // For echo responses, use the original client address from the session
                                        let echo_packet = SessionPacketBuilder::new(
                                            session_id, 
                                            original_client_addr,
                                            payload.to_vec()
                                        ).build();
                                        
                                        match socket.send_to(&echo_packet, from).await {
                                            Ok(bytes_sent) => {
                                                debug!("✅ Echoed session packet {} back to {}", session_id, from);
                                                stats_task.record_packet_sent(bytes_sent);
                                            }
                                            Err(e) => {
                                                error!("❌ Failed to echo session packet to {}: {}", from, e);
                                                stats_task.record_connection_error();
                                            }
                                        }
                                    }

                                    if let (Some(forward_addr), Some(ref target_sock)) = (forward, &target_socket) {
                                        // Forward payload (without session header) to destination
                                        match target_sock.send_to(payload, forward_addr).await {
                                            Ok(bytes_sent) => {
                                                debug!("✅ Forwarded {} bytes (session {}) to {}", 
                                                       bytes_sent, session_id, forward_addr);
                                                stats_task.record_packet_sent(bytes_sent);
                                            }
                                            Err(e) => {
                                                error!("❌ Failed to forward to {}: {}", forward_addr, e);
                                                stats_task.record_connection_error();
                                            }
                                        }
                                    }
                                    
                                    continue;
                                }
                            }

                            // Handle regular packets (non-session)
                            debug!("🔧 Handling regular (non-session) packet from {}", from);
                            
                            if echo {
                                // Echo back to sender
                                match socket.send_to(data, from).await {
                                    Ok(bytes_sent) => {
                                        debug!("✅ Echoed {} bytes back to {}", bytes_sent, from);
                                        stats_task.record_packet_sent(bytes_sent);
                                    }
                                    Err(e) => {
                                        error!("❌ Failed to echo to {}: {}", from, e);
                                        stats_task.record_connection_error();
                                    }
                                }
                            }

                            if let (Some(forward_addr), Some(ref target_sock)) = (forward, &target_socket) {
                                // Forward to destination
                                match target_sock.send_to(data, forward_addr).await {
                                    Ok(bytes_sent) => {
                                        debug!("✅ Forwarded {} bytes to {}", bytes_sent, forward_addr);
                                        stats_task.record_packet_sent(bytes_sent);
                                    }
                                    Err(e) => {
                                        error!("❌ Failed to forward to {}: {}", forward_addr, e);
                                        stats_task.record_connection_error();
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error!("❌ Server {} receive error: {}", addr, e);
                            stats_task.record_connection_error();
                        }
                    }
                }
            });
            
            server_tasks.push(task);
        }

        // Start task to receive responses from target service and send back directly to clients
        if let (Some(forward_addr), Some(target_sock)) = (self.forward, target_socket.clone()) {
            
            let session_mgr = session_manager.clone();
            let stats_target = stats.clone();
            
            if use_color {
                println!("{} Started target response listener for {}", "🔄".cyan(), forward_addr.to_string().yellow());
            } else {
                println!("🔄 Started target response listener for {}", forward_addr);
            }
            
            let target_response_task = tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                
                loop {
                    match target_sock.recv_from(&mut buf).await {
                        Ok((len, from)) => {
                            if from == forward_addr {
                                let response_data = &buf[..len];
                                debug!("📨 Received {} bytes response from target {}", len, from);
                                stats_target.record_packet_received(len);
                                
                                // Find active sessions and their aggregation clients
                                let mut response_sent = false;
                                
                                for (session_id, agg_client_addr) in session_mgr.get_active_sessions() {
                                    if let Some(original_client_addr) = session_mgr.get_original_client_addr(session_id) {
                                        // Create response packet with session header
                                        let response_packet = SessionPacketBuilder::new(
                                            session_id,
                                            original_client_addr,
                                            response_data.to_vec()
                                        ).build();
                                        
                                        // Send response directly back to the aggregation client via UDP
                                        // The aggregation client will then route it to the original client
                                        match target_sock.send_to(&response_packet, agg_client_addr).await {
                                            Ok(bytes_sent) => {
                                                debug!("✅ Sent {} bytes response to aggregation client {} for session {}", 
                                                       bytes_sent, agg_client_addr, session_id);
                                                stats_target.record_packet_sent(bytes_sent);
                                                response_sent = true;
                                                break; // Successfully sent, use only first session
                                            }
                                            Err(e) => {
                                                error!("❌ Failed to send response to aggregation client {}: {}", agg_client_addr, e);
                                            }
                                        }
                                    }
                                }
                                
                                if !response_sent {
                                    warn!("⚠️  Received response from target but no active sessions available for routing");
                                    debug!("Response payload: {} bytes from {}", len, from);
                                    stats_target.record_connection_error();
                                }
                            } else {
                                debug!("⚠️  Received unexpected response from {}, expected {}", from, forward_addr);
                            }
                        }
                        Err(e) => {
                            error!("❌ Target response receive error: {}", e);
                            stats_target.record_connection_error();
                            sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            });
            server_tasks.push(target_response_task);
        }

        // Session cleanup task
        let session_manager_cleanup = session_manager.clone();
        let session_cleanup = tokio::spawn(async move {
            let mut cleanup_interval = interval(Duration::from_secs(30));
            loop {
                cleanup_interval.tick().await;
                let expired = session_manager_cleanup.cleanup_expired_sessions();
                if expired > 0 {
                    if use_color {
                        println!("{} Cleaned up {} expired sessions", "🧹".dim(), expired.to_string().dim());
                    } else {
                        println!("🧹 Cleaned up {} expired sessions", expired);
                    }
                }
                
                let session_stats = session_manager_cleanup.stats();
                debug!("📊 Session stats: {} active sessions", session_stats.active_sessions);
            }
        });

        // Statistics reporting
        let server_count = self.listen.len();
        let stats_clone = stats.clone();
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(stats_interval));
            loop {
                interval.tick().await;
                
                print_stats(&stats_clone, use_color);
                
                if use_color {
                    println!("{} Server running with {} listeners", "🏃".cyan(), server_count.to_string().bold());
                } else {
                    println!("🏃 Server running with {} listeners", server_count);
                }
            }
        });

        // Print server ready message
        if use_color {
            println!("\n{} UDP Aggregation Server is ready!", "🌟".green().bold());
            println!("{} Listening on {} address(es)", "📍".blue(), server_count.to_string().bold());
            if self.echo {
                println!("{} Echo mode: ON", "🔊".yellow());
            }
            if let Some(forward_addr) = self.forward {
                println!("{} Forward mode: ON -> {}", "🔀".yellow(), forward_addr.to_string().cyan());
            }
            println!("{} Press Ctrl+C to stop", "💡".dim());
        } else {
            println!("\n🌟 UDP Aggregation Server is ready!");
            println!("📍 Listening on {} address(es)", server_count);
            if self.echo {
                println!("🔊 Echo mode: ON");
            }
            if let Some(forward_addr) = self.forward {
                println!("🔀 Forward mode: ON -> {}", forward_addr);
            }
            println!("💡 Press Ctrl+C to stop");
        }

        // Wait for all tasks
        server_tasks.push(session_cleanup);
        futures::future::try_join_all(server_tasks).await?;

        Ok(())
    }
}

impl TestCli {
    async fn run(self, _stats_interval: u64, cfg_path: &Option<PathBuf>) -> Result<()> {
        info!("Starting UDP aggregation performance test");

        // Load configuration from file or use defaults with CLI overrides
        let mut config = load_udp_config(cfg_path)?;

        // Override load balancing strategy if specified via CLI
        let load_balance = match self.load_balance.as_str() {
            "round-robin" => LoadBalanceStrategy::RoundRobin,
            "random" => LoadBalanceStrategy::Random,
            "weighted" => LoadBalanceStrategy::WeightedByBandwidth,
            "fastest" => LoadBalanceStrategy::FastestFirst,
            _ => bail!("Invalid load balance strategy: {}", self.load_balance),
        };
        config.load_balance = load_balance;
        
        if cfg_path.is_some() {
            info!("Configuration loaded from file");
        }

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
