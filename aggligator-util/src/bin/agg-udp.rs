//! Simple UDP aggregation tool - minimal implementation for testing multi-path UDP aggregation.
//!
//! This tool provides a basic proxy that can distribute UDP packets across multiple links
//! for bandwidth aggregation, similar to mptcp but for UDP.

use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::{style::Stylize, tty::IsTty};
use rand::{rng, Rng};
use std::{
    collections::HashMap,
    io::stdout,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    net::UdpSocket,
    sync::{broadcast, RwLock},
    time::interval,
};
use tracing::{debug, error, info, warn};

use aggligator::{id::LinkId, unordered_cfg::LoadBalanceStrategy, unordered_task::UnorderedLinkTransport};
use aggligator_transport_udp::UdpTransport;
use aggligator_util::init_log;

/// Link performance statistics
#[derive(Debug, Clone)]
struct LinkStats {
    /// Total packets sent on this link
    packets_sent: u64,
    /// Total bytes sent on this link
    bytes_sent: u64,
    /// Total packets lost (estimated)
    packets_lost: u64,
    /// Last round-trip time measurement
    last_rtt: Option<Duration>,
    /// Current bandwidth (bytes per second) - calculated over last window
    bandwidth_bps: u64,
    /// Last update timestamp
    last_update: Instant,
    /// Packet loss rate (0.0 to 1.0)
    loss_rate: f64,
    /// Bytes sent in current measurement window
    window_bytes: u64,
    /// Window start time
    window_start: Instant,
}

impl Default for LinkStats {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            packets_sent: 0,
            bytes_sent: 0,
            packets_lost: 0,
            last_rtt: None,
            bandwidth_bps: 0,
            last_update: now,
            loss_rate: 0.0,
            window_bytes: 0,
            window_start: now,
        }
    }
}

impl LinkStats {
    fn update_send_stats(&mut self, bytes: usize) {
        let now = Instant::now();
        self.packets_sent += 1;
        self.bytes_sent += bytes as u64;
        self.window_bytes += bytes as u64;

        // Update bandwidth calculation with sliding window (5 second window)
        let window_duration = now.duration_since(self.window_start);
        if window_duration >= Duration::from_secs(5) {
            // Calculate bandwidth over the window
            self.bandwidth_bps = (self.window_bytes as f64 / window_duration.as_secs_f64()) as u64;
            
            // Reset window
            self.window_bytes = 0;
            self.window_start = now;
        }
        
        self.last_update = now;
    }

    fn update_loss_stats(&mut self, lost_packets: u64) {
        self.packets_lost += lost_packets;
        if self.packets_sent > 0 {
            self.loss_rate = self.packets_lost as f64 / self.packets_sent as f64;
        }
    }

    fn get_weight_for_bandwidth(&self) -> f64 {
        if self.bandwidth_bps == 0 {
            1.0
        } else {
            self.bandwidth_bps as f64
        }
    }

    fn get_weight_for_loss(&self) -> f64 {
        // Higher weight for lower loss rate
        1.0 - self.loss_rate.min(0.99)
    }

    fn get_adaptive_weight(&self) -> f64 {
        // Combine bandwidth and loss rate with exploration factor
        let bandwidth_weight = self.get_weight_for_bandwidth();
        let loss_weight = self.get_weight_for_loss();
        let exploration_factor = 0.1; // 10% exploration to prevent link starvation

        (bandwidth_weight * loss_weight * 0.9) + exploration_factor
    }
}

/// Simple UDP aggregation CLI tool
#[derive(Debug, Parser)]
#[command(name = "agg-udp-simple")]
#[command(about = "Simple UDP multi-path aggregation tool")]
struct SimpleCli {
    #[command(subcommand)]
    command: Commands,

    /// Verbose logging
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run as client (aggregate outgoing packets)
    Client {
        /// Local bind address
        #[arg(short, long, default_value = "127.0.0.1:8000")]
        local: SocketAddr,
        /// Target servers to aggregate across
        #[arg(short, long)]
        targets: Vec<SocketAddr>,
        /// Load balancing strategy: packet-round-robin, weighted-bandwidth, fastest-first, weighted-packet-loss, dynamic-adaptive
        #[arg(long, default_value = "packet-round-robin")]
        strategy: String,
        /// Node role for directional bandwidth prioritization: client, server, balanced
        #[arg(long, default_value = "balanced")]
        role: String,
        /// Do not display the link monitor
        #[arg(long, short = 'n')]
        no_monitor: bool,
    },
    /// Run as server (collect aggregated packets)
    Server {
        /// Listen addresses (multiple interfaces for aggregation)
        #[arg(short, long)]
        listen: Vec<SocketAddr>,
        /// Target backend server
        #[arg(short, long, default_value = "127.0.0.1:9000")]
        target: SocketAddr,
        /// Do not display the link monitor
        #[arg(long, short = 'n')]
        no_monitor: bool,
    },
}

/// Simple UDP packet aggregator
struct SimpleUdpAggregator {
    /// Map of links by ID
    links: Arc<RwLock<HashMap<LinkId, Arc<UdpTransport>>>>,
    /// Load balancing strategy
    strategy: LoadBalanceStrategy,
    /// Round-robin counter
    rr_counter: AtomicU64,
    /// Statistics
    packets_sent: AtomicU64,
    bytes_sent: AtomicU64,
    /// Per-link performance statistics
    link_stats: Arc<RwLock<HashMap<LinkId, LinkStats>>>,
}

impl SimpleUdpAggregator {
    pub fn new(strategy: LoadBalanceStrategy) -> Self {
        Self {
            links: Arc::new(RwLock::new(HashMap::new())),
            strategy,
            rr_counter: AtomicU64::new(0),
            packets_sent: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            link_stats: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Add a link to the aggregator
    pub async fn add_link(&self, link_id: LinkId, transport: Arc<UdpTransport>) {
        self.links.write().await.insert(link_id, transport);
        self.link_stats.write().await.insert(link_id, LinkStats::default());
        info!("Added link {} to aggregator", link_id.0);
    }

    /// Send data using load balancing
    pub async fn send_data(&self, data: &[u8]) -> Result<()> {
        let links = self.links.read().await;
        if links.is_empty() {
            warn!("No links available for sending");
            return Ok(());
        }

        // Select link based on load balancing strategy
        let selected_link_id = self.select_link(&links).await;

        if let Some(transport) = links.get(&selected_link_id) {
            match transport.send_raw(data).await {
                Ok(_bytes) => {
                    // Update statistics
                    self.packets_sent.fetch_add(1, Ordering::Relaxed);
                    self.bytes_sent.fetch_add(data.len() as u64, Ordering::Relaxed);

                    // Update per-link statistics
                    if let Some(stats) = self.link_stats.write().await.get_mut(&selected_link_id) {
                        stats.update_send_stats(data.len());
                    }

                    debug!("Sent {} bytes via link {} using strategy {:?}", data.len(), selected_link_id.0, self.strategy);
                }
                Err(e) => {
                    error!("Failed to send via link {}: {}", selected_link_id.0, e);

                    // Update loss statistics
                    if let Some(stats) = self.link_stats.write().await.get_mut(&selected_link_id) {
                        stats.update_loss_stats(1);
                    }
                }
            }
        } else {
            error!("Selected link {} not found in links map", selected_link_id.0);
        }

        Ok(())
    }

    /// Select a link based on the configured load balancing strategy
    async fn select_link(&self, links: &HashMap<LinkId, Arc<UdpTransport>>) -> LinkId {
        match self.strategy {
            LoadBalanceStrategy::PacketRoundRobin => {
                let count = self.rr_counter.fetch_add(1, Ordering::Relaxed);
                let index = (count as usize) % links.len();
                *links.keys().nth(index).unwrap()
            }

            LoadBalanceStrategy::WeightedByBandwidth => self.select_weighted_by_bandwidth(links).await,

            LoadBalanceStrategy::FastestFirst => self.select_fastest_link(links).await,

            LoadBalanceStrategy::WeightedByPacketLoss => self.select_weighted_by_loss(links).await,

            LoadBalanceStrategy::DynamicAdaptive => self.select_adaptive(links).await,
        }
    }

    /// Select link weighted by bandwidth
    async fn select_weighted_by_bandwidth(&self, links: &HashMap<LinkId, Arc<UdpTransport>>) -> LinkId {
        let stats = self.link_stats.read().await;
        let mut best_link = *links.keys().next().unwrap();
        let mut best_bandwidth = 0u64;

        for link_id in links.keys() {
            let bandwidth = if let Some(link_stats) = stats.get(link_id) {
                link_stats.bandwidth_bps
            } else {
                1000000 // Default 1 Mbps for new links
            };
            
            if bandwidth > best_bandwidth {
                best_bandwidth = bandwidth;
                best_link = *link_id;
            }
        }

        best_link
    }

    /// Select fastest (lowest latency) link
    async fn select_fastest_link(&self, links: &HashMap<LinkId, Arc<UdpTransport>>) -> LinkId {
        let stats = self.link_stats.read().await;
        let mut best_link = *links.keys().next().unwrap();
        let mut best_rtt = Duration::from_secs(u64::MAX);

        for link_id in links.keys() {
            let rtt = if let Some(link_stats) = stats.get(link_id) {
                link_stats.last_rtt.unwrap_or(Duration::from_millis(100)) // Default 100ms
            } else {
                Duration::from_millis(50) // New links get priority with 50ms default
            };
            
            if rtt < best_rtt {
                best_rtt = rtt;
                best_link = *link_id;
            }
        }

        best_link
    }

    /// Select link weighted by packet loss (favor low loss links)
    async fn select_weighted_by_loss(&self, links: &HashMap<LinkId, Arc<UdpTransport>>) -> LinkId {
        let stats = self.link_stats.read().await;
        let mut best_link = *links.keys().next().unwrap();
        let mut best_weight = 0.0f64;

        for link_id in links.keys() {
            let weight = if let Some(link_stats) = stats.get(link_id) {
                link_stats.get_weight_for_loss()
            } else {
                1.0 // New links get full weight
            };
            
            if weight > best_weight {
                best_weight = weight;
                best_link = *link_id;
            }
        }

        best_link
    }

    /// Dynamic adaptive selection combining multiple factors
    async fn select_adaptive(&self, links: &HashMap<LinkId, Arc<UdpTransport>>) -> LinkId {
        let stats = self.link_stats.read().await;
        let mut weights: Vec<(LinkId, f64)> = Vec::new();
        let mut total_weight = 0.0f64;

        // Calculate weights for all links
        for link_id in links.keys() {
            let weight = if let Some(link_stats) = stats.get(link_id) {
                link_stats.get_adaptive_weight()
            } else {
                1.5 // Give new links slightly higher weight for exploration
            };
            weights.push((*link_id, weight));
            total_weight += weight;
        }

        if total_weight == 0.0 {
            // Fallback to round-robin if no weights
            let count = self.rr_counter.fetch_add(1, Ordering::Relaxed);
            let index = (count as usize) % links.len();
            return *links.keys().nth(index).unwrap();
        }

        // Weighted random selection
        let mut rng = rng();
        let mut random_value = rng.random::<f64>() * total_weight;

        for (link_id, weight) in weights {
            random_value -= weight;
            if random_value <= 0.0 {
                return link_id;
            }
        }

        // Fallback (should not reach here)
        *links.keys().next().unwrap()
    }

    /// Get statistics
    pub fn stats(&self) -> (u64, u64) {
        (self.packets_sent.load(Ordering::Relaxed), self.bytes_sent.load(Ordering::Relaxed))
    }

    /// Get detailed link statistics
    pub async fn link_stats(&self) -> HashMap<LinkId, LinkStats> {
        self.link_stats.read().await.clone()
    }
}

/// UDP session for tracking client connections
#[derive(Debug)]
struct UdpSession {
    client_addr: SocketAddr,
    last_activity: Instant,
    sequence: AtomicU64,
}

impl Clone for UdpSession {
    fn clone(&self) -> Self {
        Self {
            client_addr: self.client_addr,
            last_activity: self.last_activity,
            sequence: AtomicU64::new(self.sequence.load(Ordering::Relaxed)),
        }
    }
}

impl UdpSession {
    fn new(client_addr: SocketAddr) -> Self {
        Self { 
            client_addr, 
            last_activity: Instant::now(), 
            sequence: AtomicU64::new(0),
        }
    }

    fn update_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    fn is_expired(&self, timeout: Duration) -> bool {
        self.last_activity.elapsed() > timeout
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = SimpleCli::parse();

    if cli.verbose {
        tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).init();
    } else {
        init_log();
    }

    match cli.command {
        Commands::Client { local, targets, strategy, role, no_monitor } => {
            run_client(local, targets, strategy, role, no_monitor).await
        }
        Commands::Server { listen, target, no_monitor } => run_server(listen, target, no_monitor).await,
    }
}

async fn run_client(
    local: SocketAddr, targets: Vec<SocketAddr>, strategy: String, role: String, no_monitor: bool,
) -> Result<()> {
    let no_monitor = no_monitor || !stdout().is_tty();

    info!("Starting UDP transparent proxy on {}", local);
    info!("Targets: {:?}", targets);
    info!("Strategy: {}", strategy);
    info!("Role: {}", role);

    let strategy = match strategy.as_str() {
        "packet-round-robin" => LoadBalanceStrategy::PacketRoundRobin,
        "weighted-bandwidth" => LoadBalanceStrategy::WeightedByBandwidth,
        "fastest-first" => LoadBalanceStrategy::FastestFirst,
        "weighted-packet-loss" => LoadBalanceStrategy::WeightedByPacketLoss,
        "dynamic-adaptive" => LoadBalanceStrategy::DynamicAdaptive,
        _ => LoadBalanceStrategy::PacketRoundRobin, // Default to packet-level round-robin
    };

    // Create aggregator
    let aggregator = Arc::new(SimpleUdpAggregator::new(strategy));

    // Session tracking for UDP connections
    let sessions: Arc<RwLock<HashMap<SocketAddr, UdpSession>>> = Arc::new(RwLock::new(HashMap::new()));
    let session_timeout = Duration::from_secs(60); // 60 second timeout

    // Set up links to all targets using system-selected local addresses
    let mut receive_sockets = Vec::new();
    for (i, target) in targets.iter().enumerate() {
        // Let the system choose the appropriate local interface by using new_unbound
        match UdpTransport::new_unbound(*target).await {
            Ok(transport) => {
                let transport = Arc::new(transport);

                // Store socket for receiving responses
                let recv_socket = transport.socket();
                receive_sockets.push((recv_socket, *target, LinkId(i as u128)));

                aggregator.add_link(LinkId(i as u128), transport).await;
                info!("Connected to target {} with auto-selected local address", target);
            }
            Err(e) => {
                error!("Failed to connect to target {}: {}", target, e);
                return Err(e.into());
            }
        }
    }

    // Bind local socket for receiving client requests
    let client_socket = Arc::new(UdpSocket::bind(local).await?);
    info!("Transparent proxy listening on {}", local);

    // Create monitoring channels
    let (control_tx, control_rx) = broadcast::channel::<String>(100);

    // Statistics
    let stats_aggregator = aggregator.clone();
    let stats_control_tx = control_tx.clone();
    let stats_task = tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            let (packets, bytes) = stats_aggregator.stats();
            let link_stats = stats_aggregator.link_stats().await;
            
            // Create detailed status message with per-link statistics
            let mut status = format!("UDP Proxy - {} packets, {} bytes forwarded\n", packets, bytes);
            
            if !link_stats.is_empty() {
                status.push_str("Link Details:\n");
                for (link_id, stats) in link_stats.iter() {
                    let bandwidth_kbps = if stats.bandwidth_bps > 0 {
                        stats.bandwidth_bps / 1024
                    } else {
                        0
                    };
                    let loss_percentage = (stats.loss_rate * 100.0) as u32;
                    
                    status.push_str(&format!(
                        "  Link {}: {} pkts, {} KB, {} KB/s, {}% loss\n",
                        link_id.0,
                        stats.packets_sent,
                        stats.bytes_sent / 1024,
                        bandwidth_kbps,
                        loss_percentage
                    ));
                }
            }
            
            let _ = stats_control_tx.send(status);

            if !no_monitor {
                debug!("Stats: {} packets, {} bytes forwarded", packets, bytes);
            } else {
                info!("Stats: {} packets, {} bytes forwarded", packets, bytes);
            }
        }
    });

    // Session cleanup task
    let sessions_cleanup = sessions.clone();
    let cleanup_task = tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            let mut sessions = sessions_cleanup.write().await;
            sessions.retain(|_, session| !session.is_expired(session_timeout));
            debug!("Active sessions: {}", sessions.len());
        }
    });

    // Client to server forwarding task
    let client_sessions = sessions.clone();
    let forward_aggregator = aggregator.clone();
    let forward_socket = client_socket.clone();
    let forward_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            match forward_socket.recv_from(&mut buf).await {
                Ok((len, client_addr)) => {
                    debug!("Received {} bytes from client {}", len, client_addr);

                    // Update or create session
                    {
                        let mut sessions = client_sessions.write().await;
                        match sessions.get_mut(&client_addr) {
                            Some(session) => {
                                session.update_activity();
                            }
                            None => {
                                sessions.insert(client_addr, UdpSession::new(client_addr));
                                info!("New client session: {}", client_addr);
                            }
                        }
                    }

                    // Forward to aggregated targets
                    if let Err(e) = forward_aggregator.send_data(&buf[..len]).await {
                        error!("Failed to forward packet from {}: {}", client_addr, e);
                    }
                }
                Err(e) => {
                    error!("Failed to receive from client: {}", e);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    });

    // Server to client response handling tasks
    let mut response_tasks = Vec::new();
    for (recv_socket, target_addr, link_id) in receive_sockets {
        let sessions_resp = sessions.clone();
        let client_socket_resp = client_socket.clone();
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            info!("Listening for responses from {} (link {})", target_addr, link_id.0);

            loop {
                match recv_socket.recv(&mut buf).await {
                    Ok(len) => {
                        debug!("Received {} bytes response from {} (link {})", len, target_addr, link_id.0);

                        // Forward response to all active clients
                        // In a more sophisticated implementation, you'd track which client
                        // sent the original request and only send to that client
                        let sessions = sessions_resp.read().await;
                        for (client_addr, _session) in sessions.iter() {
                            if let Err(e) = client_socket_resp.send_to(&buf[..len], client_addr).await {
                                debug!("Failed to send response to client {}: {}", client_addr, e);
                            } else {
                                debug!("Forwarded response to client {}", client_addr);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to receive response from {} (link {}): {}", target_addr, link_id.0, e);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        });
        response_tasks.push(task);
    }

    let title = format!("UDP Transparent Proxy - {} -> {:?}", local, targets);

    if no_monitor {
        drop(control_rx);
        eprintln!("{}", title);

        // Wait for all tasks
        tokio::select! {
            _ = stats_task => {},
            _ = cleanup_task => {},
            _ = forward_task => {},
            _ = futures::future::join_all(response_tasks) => {},
        }
    } else {
        eprintln!("{}", title.clone().bold());

        // Simple monitoring
        let title_monitor = title.clone();
        let monitor_task = tokio::spawn(async move {
            let mut control_rx = control_rx;
            loop {
                match control_rx.recv().await {
                    Ok(status) => {
                        // Clear previous output and print new status
                        eprint!("\x1B[2J\x1B[H"); // Clear screen and move cursor to top
                        eprint!("{}", title_monitor.clone().bold());
                        eprint!("\n{}", status);
                    }
                    Err(_) => break,
                }
            }
        });

        tokio::select! {
            _ = stats_task => {},
            _ = cleanup_task => {},
            _ = forward_task => {},
            _ = monitor_task => {},
            _ = futures::future::join_all(response_tasks) => {},
        }
    }

    Ok(())
}

async fn run_server(listen_addrs: Vec<SocketAddr>, target: SocketAddr, no_monitor: bool) -> Result<()> {
    let no_monitor = no_monitor || !stdout().is_tty();

    info!("Starting UDP aggregation server");
    info!("Listen addresses: {:?}", listen_addrs);
    info!("Target backend: {}", target);

    // Create monitoring channels - simplified for UDP
    let (control_tx, control_rx) = broadcast::channel::<String>(100);

    let mut handles = Vec::new();
    let stats = Arc::new(AtomicU64::new(0));

    // Create a listener for each address
    for listen_addr in listen_addrs.clone() {
        let target = target;
        let control_tx = control_tx.clone();
        let stats = stats.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = run_server_instance(listen_addr, target, control_tx, stats).await {
                error!("Server instance {} failed: {}", listen_addr, e);
            }
        });
        handles.push(handle);
    }

    let title = format!("UDP Aggregation Server - {:?} -> {}", listen_addrs, target);

    if no_monitor {
        drop(control_rx);
        eprintln!("{}", title);

        // Wait for all instances
        for handle in handles {
            if let Err(e) = handle.await {
                error!("Server handle failed: {}", e);
            }
        }
    } else {
        eprintln!("{}", title.bold());

        // Statistics task
        let stats_clone = stats.clone();
        let stats_control_tx = control_tx.clone();
        let stats_task = tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let packets = stats_clone.load(Ordering::Relaxed);
                let status = format!("UDP Server - {} packets forwarded", packets);
                let _ = stats_control_tx.send(status);
            }
        });

        // Simple monitoring without interactive_monitor
        let monitor_task = tokio::spawn(async move {
            let mut control_rx = control_rx;
            loop {
                match control_rx.recv().await {
                    Ok(status) => {
                        eprintln!("{}", status);
                    }
                    Err(_) => break,
                }
            }
        });

        let server_task = tokio::spawn(async move {
            // Wait for all instances
            for handle in handles {
                if let Err(e) = handle.await {
                    error!("Server handle failed: {}", e);
                }
            }
        });

        tokio::select! {
            _ = stats_task => {},
            _ = server_task => {},
            _ = monitor_task => {},
        }
    }

    Ok(())
}

async fn run_server_instance(
    listen_addr: SocketAddr, target: SocketAddr, control_tx: broadcast::Sender<String>, stats: Arc<AtomicU64>,
) -> Result<()> {
    let socket = Arc::new(UdpSocket::bind(listen_addr).await?);
    info!("Server listening on {} -> {}", listen_addr, target);

    // Create backend connection for forwarding
    let backend_socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    backend_socket.connect(target).await?;
    info!("Connected to backend {} from {}", target, backend_socket.local_addr()?);

    // Session tracking for server side
    let sessions: Arc<RwLock<HashMap<SocketAddr, Instant>>> = Arc::new(RwLock::new(HashMap::new()));
    let session_timeout = Duration::from_secs(60);

    // Client to backend forwarding task
    let forward_socket = socket.clone();
    let forward_backend = backend_socket.clone();
    let forward_sessions = sessions.clone();
    let forward_stats = stats.clone();
    let forward_control_tx = control_tx.clone();
    let forward_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        let mut packet_count = 0u64;

        loop {
            match forward_socket.recv_from(&mut buf).await {
                Ok((len, proxy_addr)) => {
                    packet_count += 1;
                    forward_stats.fetch_add(1, Ordering::Relaxed);

                    debug!(
                        "Server {} received {} bytes from proxy {} (packet #{})",
                        listen_addr, len, proxy_addr, packet_count
                    );

                    // Update session
                    {
                        let mut sessions = forward_sessions.write().await;
                        sessions.insert(proxy_addr, Instant::now());
                    }

                    // Forward to backend
                    match forward_backend.send(&buf[..len]).await {
                        Ok(_) => {
                            debug!("Forwarded {} bytes to backend {}", len, target);
                        }
                        Err(e) => {
                            error!("Failed to forward to backend {}: {}", target, e);
                        }
                    }

                    // Send status update for monitoring
                    if packet_count % 100 == 0 {
                        let status = format!("{}: forwarded {} packets to backend", listen_addr, packet_count);
                        let _ = forward_control_tx.send(status);
                    }
                }
                Err(e) => {
                    error!("Server {} receive error: {}", listen_addr, e);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    });

    // Backend to client response forwarding task
    let response_socket = socket.clone();
    let response_backend = backend_socket.clone();
    let response_sessions = sessions.clone();
    let response_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];

        loop {
            match response_backend.recv(&mut buf).await {
                Ok(len) => {
                    debug!("Received {} bytes response from backend {}", len, target);

                    // Forward response to corresponding proxy clients
                    let sessions = response_sessions.read().await;
                    for (proxy_addr, last_seen) in sessions.iter() {
                        if last_seen.elapsed() <= session_timeout {
                            if let Err(e) = response_socket.send_to(&buf[..len], proxy_addr).await {
                                debug!("Failed to send response to proxy {}: {}", proxy_addr, e);
                            } else {
                                debug!("Forwarded response to proxy {}", proxy_addr);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to receive from backend {}: {}", target, e);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    });

    // Session cleanup task
    let cleanup_sessions = sessions.clone();
    let cleanup_task = tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            let mut sessions = cleanup_sessions.write().await;
            sessions.retain(|_, last_seen| last_seen.elapsed() <= session_timeout);
            debug!("Server {}: Active sessions: {}", listen_addr, sessions.len());
        }
    });

    tokio::select! {
        _ = forward_task => {},
        _ = response_task => {},
        _ = cleanup_task => {},
    }

    Ok(())
}
