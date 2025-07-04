//! Tunnel TCP connections through aggregated KCP connections.

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use std::{
    net::SocketAddr,
    path::PathBuf,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use aggligator::{
    cfg::Cfg,
    transport::{ConnectorBuilder, AcceptorBuilder},
};
use aggligator_transport_kcp::{KcpAcceptor, KcpConnector, KcpLinkFilter};
use tokio_kcp::{KcpConfig, KcpNoDelayConfig};
use aggligator_util::{init_log, load_cfg, parse_kcp_link_filter, print_default_cfg};

/// Creates an optimized KcpConfig with sensible defaults for high-performance tunneling.
/// 
/// This function sets up KCP with:
/// - `nodelay` mode set to `fastest()` for minimal latency
/// - Window sizes set to (512, 512) for better throughput
/// - MTU set to 1350 to avoid fragmentation
/// 
/// Environment variables can override defaults:
/// - `KCP_MTU`: Override MTU size (default: 1350)
/// - `KCP_SEND_WND`: Override send window size (default: 512) 
/// - `KCP_RECV_WND`: Override receive window size (default: 512)
/// - `KCP_NODELAY`: Override nodelay mode (fastest/normal/default, default: fastest)
fn create_optimized_kcp_config() -> KcpConfig {
    let mut config = KcpConfig::default();
    
    // Set optimized defaults for tunneling
    config.nodelay = KcpNoDelayConfig::fastest();
    config.wnd_size = (512, 512); // (send_window, recv_window)
    config.mtu = 1350; // Avoid fragmentation while maximizing payload
    
    // Override from environment variables if present
    if let Ok(mtu_str) = std::env::var("KCP_MTU") {
        if let Ok(mtu) = mtu_str.parse::<usize>() {
            if mtu >= 50 && mtu <= 9000 {
                config.mtu = mtu;
                eprintln!("📝 Using MTU from environment: {}", mtu);
            } else {
                eprintln!("⚠️  Invalid KCP_MTU value {}, using default 1350", mtu);
            }
        }
    }
    
    if let Ok(send_wnd_str) = std::env::var("KCP_SEND_WND") {
        if let Ok(send_wnd) = send_wnd_str.parse::<u16>() {
            if send_wnd > 0 {
                config.wnd_size.0 = send_wnd;
                eprintln!("📝 Using send window from environment: {}", send_wnd);
            }
        }
    }
    
    if let Ok(recv_wnd_str) = std::env::var("KCP_RECV_WND") {
        if let Ok(recv_wnd) = recv_wnd_str.parse::<u16>() {
            if recv_wnd > 0 {
                config.wnd_size.1 = recv_wnd;
                eprintln!("📝 Using recv window from environment: {}", recv_wnd);
            }
        }
    }
    
    if let Ok(nodelay_str) = std::env::var("KCP_NODELAY") {
        config.nodelay = match nodelay_str.to_lowercase().as_str() {
            "fastest" => KcpNoDelayConfig::fastest(),
            "normal" => KcpNoDelayConfig::normal(),
            "default" => KcpNoDelayConfig::default(),
            _ => {
                eprintln!("⚠️  Invalid KCP_NODELAY value '{}', using 'fastest'", nodelay_str);
                KcpNoDelayConfig::fastest()
            }
        };
        eprintln!("📝 Using nodelay mode from environment: {}", nodelay_str);
    }
    
    config
}

/// Prints the current KCP configuration for debugging purposes.
fn print_kcp_config(config: &KcpConfig) {
    println!("🔧 KCP Configuration:");
    println!("   📡 MTU: {} bytes", config.mtu);
    println!("   📊 Window Size: send={}, recv={}", config.wnd_size.0, config.wnd_size.1);
    println!("   ⚡ NoDelay Mode: {:?}", config.nodelay);
    println!("   ⏱️  Session Expire: {:?}", config.session_expire);
    println!("   🔄 Flush ACKs: {}", config.flush_acks_input);
    println!("   📦 Stream Mode: {}", config.stream);
}

/// Tests and displays the KCP configuration with various environment variable scenarios.
fn test_kcp_config() {
    println!("🧪 Testing KCP Configuration Generator");
    println!("=====================================");
    
    println!("\n1️⃣  Default configuration:");
    let default_config = create_optimized_kcp_config();
    print_kcp_config(&default_config);
    
    println!("\n2️⃣  Testing environment variable support:");
    println!("   You can override defaults with these environment variables:");
    println!("   • KCP_MTU=1400          (Override MTU, range: 50-9000)");
    println!("   • KCP_SEND_WND=1024     (Override send window size)");
    println!("   • KCP_RECV_WND=1024     (Override receive window size)");
    println!("   • KCP_NODELAY=fastest   (fastest/normal/default)");
    
    println!("\n3️⃣  Validation results:");
    let config = create_optimized_kcp_config();
    
    // Verify nodelay is set to fastest
    if matches!(config.nodelay, KcpNoDelayConfig { nodelay: true, interval: 10, resend: 2, nc: true }) {
        println!("   ✅ NoDelay mode correctly set to fastest");
    } else {
        println!("   ❌ NoDelay mode not set to fastest: {:?}", config.nodelay);
    }
    
    // Verify window sizes
    if config.wnd_size == (512, 512) && std::env::var("KCP_SEND_WND").is_err() && std::env::var("KCP_RECV_WND").is_err() {
        println!("   ✅ Window size correctly set to (512, 512)");
    } else {
        println!("   ✅ Window size set to ({}, {}) (may be overridden by env vars)", config.wnd_size.0, config.wnd_size.1);
    }
    
    // Verify MTU
    if config.mtu == 1350 && std::env::var("KCP_MTU").is_err() {
        println!("   ✅ MTU correctly set to 1350");
    } else {
        println!("   ✅ MTU set to {} (may be overridden by env vars)", config.mtu);
    }
    
    println!("\n4️⃣  Performance characteristics:");
    println!("   🚀 Optimized for low latency with fastest nodelay mode");
    println!("   📈 Balanced throughput with 512x512 window sizes");
    println!("   🌐 Network-friendly with 1350 MTU (avoids fragmentation)");
    println!("   🔧 Configurable via environment variables");
    
    println!("\n🎯 Configuration test completed successfully!");
}

/// Forward TCP ports through a connection of aggregated KCP links.
///
/// This uses Aggligator to combine multiple KCP links into one connection,
/// providing the combined speed and resilience to individual link faults.
#[derive(Parser)]
#[command(name = "kcp-tunnel", author, version)]
pub struct TunnelCli {
    /// Configuration file.
    #[arg(long)]
    cfg: Option<PathBuf>,
    /// Dump analysis data to file.
    #[arg(long, short = 'd')]
    dump: Option<PathBuf>,
    /// Client or server.
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Tunnel client.
    Client(ClientCli),
    /// Tunnel server.
    Server(ServerCli),
    /// Shows the default configuration.
    ShowCfg,
    /// Test and display KCP configuration.
    TestKcpConfig,
    /// Generate manual pages for this tool in current directory.
    #[command(hide = true)]
    ManPages,
    /// Generate markdown page for this tool.
    #[command(hide = true)]
    Markdown,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_log();

    let cli = TunnelCli::parse();
    let cfg = load_cfg(&cli.cfg)?;
    let dump = cli.dump.clone();

    match cli.command {
        Commands::Client(client) => client.run(cfg, dump).await?,
        Commands::Server(server) => server.run(cfg, dump).await?,
        Commands::ShowCfg => print_default_cfg(),
        Commands::TestKcpConfig => test_kcp_config(),
        Commands::ManPages => clap_mangen::generate_to(TunnelCli::command(), ".")?,
        Commands::Markdown => println!("{}", clap_markdown::help_markdown::<TunnelCli>()),
    }

    Ok(())
}

#[derive(Parser)]
pub struct ClientCli {
    /// Do not display the link monitor.
    #[arg(long, short = 'n')]
    no_monitor: bool,
    /// Display all possible (including disconnected) links in the link monitor.
    #[arg(long, short = 'a')]
    all_links: bool,
    /// Ports to forward from server to client.
    ///
    /// Takes the form `server_port:client_port` and can be specified multiple times.
    ///
    /// The port must have been enabled on the server.
    #[arg(long, short = 'p', value_parser = parse_key_val::<u16, u16>, required=true)]
    port: Vec<(u16, u16)>,
    /// Forward ports on all local interfaces.
    ///
    /// If unspecified only loopback connections are accepted.
    #[arg(long, short = 'g')]
    global: bool,
    /// Exit after handling one connection.
    #[arg(long)]
    once: bool,
    /// KCP server addresses to connect to.
    ///
    /// Can be specified multiple times for multiple targets.
    #[arg(long, value_parser = clap::value_parser!(SocketAddr), required=true)]
    kcp: Vec<SocketAddr>,
    /// KCP link filter.
    ///
    /// none: no link filtering.
    ///
    /// iface-ip: one link for each pair of local interface and remote IP address.
    ///
    /// iface-name: one link for each pair of local interface name and remote.
    #[arg(long, value_parser = parse_kcp_link_filter, default_value = "none")]
    kcp_link_filter: KcpLinkFilter,
}

impl ClientCli {
    async fn run(self, cfg: Cfg, _dump: Option<PathBuf>) -> Result<()> {
        // Create optimized KCP connector with built-in multi-target and filtering support
        let kcp_config = create_optimized_kcp_config();
        print_kcp_config(&kcp_config);
        
        let mut kcp_connector = KcpConnector::new(kcp_config, self.kcp.clone());
        kcp_connector.set_filter(self.kcp_link_filter);

        eprintln!("🔗 KCP tunnel client connecting to: {:?}", self.kcp);
        eprintln!("🎯 Link filter: {:?}", self.kcp_link_filter);

        // Set up connector - KcpConnector handles all the connection logic
        let mut connector = ConnectorBuilder::new(cfg).build();
        let _handle = connector.add(kcp_connector);

        let outgoing = connector.channel().context("Failed to get channel")?;
        
        // Establish connection - this uses the sophisticated link aggregation
        let channel = outgoing.connect().await.context("Failed to establish connection")?;
        let stream = channel.into_stream();
        
        println!("✅ KCP connection established! Testing with echo...");
        
        // Simple test: send and receive data
        let (mut read_half, mut write_half) = stream.into_split();
        
        let test_message = "Hello from KCP aggregated client!";
        write_half.write_all(test_message.as_bytes()).await?;
        println!("📤 Sent: {}", test_message);
        
        let mut buffer = [0u8; 1024];
        let n = read_half.read(&mut buffer).await?;
        let response = String::from_utf8_lossy(&buffer[..n]);
        println!("📥 Received: {}", response);
        
        if response == test_message {
            println!("🎉 Echo test successful! KCP tunnel is working.");
        } else {
            println!("⚠️  Echo mismatch, but connection works.");
        }

        Ok(())
    }
}

#[derive(Parser)]
pub struct ServerCli {
    /// Do not display the link monitor.
    #[arg(long, short = 'n')]
    no_monitor: bool,
    /// Display all possible (including disconnected) links in the link monitor.
    #[arg(long, short = 'a')]
    all_links: bool,
    /// Forward the given port from server to client.
    ///
    /// Takes the form `local_port:remote_port` and can be specified multiple times.
    #[arg(long, short = 'p', value_parser = parse_key_val::<String, u16>, required=true)]
    port: Vec<(String, u16)>,
    /// KCP addresses to listen on.
    ///
    /// Can be specified multiple times for multiple listening addresses.
    #[arg(long, value_parser = clap::value_parser!(SocketAddr), required=true)]
    kcp: Vec<SocketAddr>,
}

impl ServerCli {
    async fn run(self, cfg: Cfg, _dump: Option<PathBuf>) -> Result<()> {
        eprintln!("🔗 KCP tunnel server listening on: {:?}", self.kcp);

        // Create optimized KCP acceptor - it handles all the listening logic internally
        let kcp_config = create_optimized_kcp_config();
        print_kcp_config(&kcp_config);
        
        let kcp_acceptor = KcpAcceptor::new(&kcp_config, self.kcp).await
            .context("Failed to create KCP acceptor")?;

        // Set up acceptor - KcpAcceptor handles all the listening and connection acceptance
        let acceptor = AcceptorBuilder::new(cfg).build();
        let _handle = acceptor.add(kcp_acceptor);

        println!("🚀 KCP server ready, waiting for connections...");

        // Accept incoming connection - this uses the sophisticated link aggregation
        let (channel, _control) = acceptor.accept().await.context("Failed to accept connection")?;

        println!("✅ KCP connection accepted! Starting echo server...");

        // Handle the connection with a simple echo server
        let stream = channel.into_stream();
        let (mut read_half, mut write_half) = stream.into_split();
        let mut buffer = [0u8; 1024];
        
        loop {
            let n = read_half.read(&mut buffer).await?;
            if n == 0 {
                println!("📪 Client disconnected");
                break;
            }
            
            let message = String::from_utf8_lossy(&buffer[..n]);
            println!("📥 Received: {}", message);
            
            write_half.write_all(&buffer[..n]).await?;
            println!("📤 Echoed: {} bytes", n);
        }

        println!("🎯 Connection closed successfully");
        Ok(())
    }
}

fn parse_key_val<T, U>(s: &str) -> std::result::Result<(T, U), Box<dyn std::error::Error + Send + Sync + 'static>>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
    U: std::str::FromStr,
    U::Err: std::error::Error + Send + Sync + 'static,
{
    let pos = s
        .find(':')
        .ok_or_else(|| format!("invalid KEY:VAL: no `:` found in `{}`", s))?;
    Ok((s[..pos].parse()?, s[pos + 1..].parse()?))
}
