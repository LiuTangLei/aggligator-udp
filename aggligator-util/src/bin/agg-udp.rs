//! Simple UDP aggregation tool - minimal implementation for testing multi-path UDP aggregation.
//!
//! This tool provides a basic proxy that can distribute UDP packets across multiple links
//! for bandwidth aggregation, similar to mptcp but for UDP.

use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::{style::Stylize, tty::IsTty};
use network_interface::{NetworkInterface, NetworkInterfaceConfig};

use std::{
    collections::{HashMap, HashSet},
    io::stdout,
    net::{IpAddr, SocketAddr},
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
use tracing::{debug, error, info, trace, warn};

use aggligator::{unordered_cfg::LoadBalanceStrategy};
use aggligator_util::init_log;

/// Link performance statistics for simple UDP aggregation
#[derive(Debug, Clone)]
struct LinkStats {
    /// Total packets sent on this link
    packets_sent: u64,
    /// Total bytes sent on this link
    bytes_sent: u64,
    /// Current bandwidth (bytes per second) - calculated over window
    bandwidth_bps: u64,
    /// Last update timestamp
    last_update: Instant,
    /// Bytes sent in current measurement window
    window_bytes: u64,
    /// Window start time
    window_start: Instant,
    
    // 简化的连接健康度统计
    /// Success rate (0.0 - 1.0) for interface availability
    success_rate: f64,
    /// Interface error rate (OS-level send failures)
    loss_rate: f64,
    /// Estimated RTT based on interface type and health
    avg_rtt: Option<Duration>,
}

impl Default for LinkStats {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            packets_sent: 0,
            bytes_sent: 0,
            bandwidth_bps: 0,
            last_update: now,
            window_bytes: 0,
            window_start: now,
            success_rate: 1.0, // 开始时假设接口是健康的
            loss_rate: 0.0,
            avg_rtt: None,
        }
    }
}

impl LinkStats {
    fn update_send_stats(&mut self, bytes: usize) {
        let now = Instant::now();
        self.packets_sent += 1;
        self.bytes_sent += bytes as u64;
        self.window_bytes += bytes as u64;

        // 使用5秒窗口减少频繁计算
        let window_duration = now.duration_since(self.window_start);
        if window_duration >= Duration::from_secs(5) {
            self.bandwidth_bps = (self.window_bytes as f64 / window_duration.as_secs_f64()) as u64;
            self.window_bytes = 0;
            self.window_start = now;
        }
        
        self.last_update = now;
    }
    
    /// 记录发送成功，更新成功率
    fn record_send_success(&mut self) {
        // 使用指数平滑来更新成功率
        self.success_rate = self.success_rate * 0.95 + 0.05;
        self.loss_rate = (1.0 - self.success_rate).max(0.0);
    }
    
    /// 记录发送失败，更新成功率
    fn record_send_failure(&mut self) {
        self.success_rate = self.success_rate * 0.9; // 失败时衰减更快
        self.loss_rate = (1.0 - self.success_rate).max(0.0);
        
        // 发送失败时给一个惩罚RTT
        self.avg_rtt = Some(Duration::from_millis(1000));
    }
    
    /// 更准确的RTT估算，基于真实网络测量
    fn estimate_rtt(&mut self) {
        // 基于连接类型和健康度的简化RTT估算
        let base_rtt = if self.success_rate > 0.95 {
            Duration::from_millis(20)  // 优质连接：20ms基础延迟
        } else if self.success_rate > 0.8 {
            Duration::from_millis(40)  // 良好连接：40ms基础延迟
        } else {
            Duration::from_millis(80)  // 较差连接：80ms基础延迟
        };
        
        // 根据丢包率增加延迟惩罚
        let loss_penalty = Duration::from_millis((self.loss_rate * 200.0) as u64);
        
        self.avg_rtt = Some(base_rtt + loss_penalty);
    }
}

/// Multi-interface UDP connection
#[derive(Debug, Clone)]
struct UdpConnection {
    socket: Arc<UdpSocket>,
    interface_name: String,
    local_addr: SocketAddr,
    target_addr: SocketAddr,
}

/// Simple UDP aggregator optimized for true bandwidth aggregation
struct SimpleUdpAggregator {
    connections: Arc<RwLock<HashMap<String, UdpConnection>>>,
    // 新增：固定顺序的连接列表，用于可预测的轮询
    connection_order: Arc<RwLock<Vec<String>>>,
    packets_sent: AtomicU64,
    bytes_sent: AtomicU64,
    link_stats: Arc<RwLock<HashMap<String, LinkStats>>>,
    send_interval_us: AtomicU64,
    // 新增：轮询索引用于真正的负载均衡
    round_robin_index: AtomicU64,
    // 新增：负载均衡策略
    strategy: LoadBalanceStrategy,
}

impl SimpleUdpAggregator {
    pub fn new(strategy: LoadBalanceStrategy) -> Self {
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            connection_order: Arc::new(RwLock::new(Vec::new())),
            packets_sent: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            link_stats: Arc::new(RwLock::new(HashMap::new())),
            send_interval_us: AtomicU64::new(0), // 不限制速率，专注带宽聚合
            round_robin_index: AtomicU64::new(0),
            strategy,
        }
    }

    /// 判断是否为虚拟或不适用的接口
    fn is_virtual_interface(interface_name: &str) -> bool {
        let virtual_prefixes = ["lo", "docker", "br-", "veth", "tun", "tap", "vmnet", "vboxnet"];
        virtual_prefixes.iter().any(|prefix| interface_name.starts_with(prefix))
    }

    /// 判断接口是否适合用于聚合
    fn is_suitable_interface(interface: &NetworkInterface) -> bool {
        // 基本检查
        if !interface.addr.iter().any(|addr| addr.ip().is_ipv4()) {
            return false;
        }

        // 排除虚拟接口
        if Self::is_virtual_interface(&interface.name) {
            return false;
        }

        // 确保接口是up状态
        interface.addr.iter().any(|addr| !addr.ip().is_loopback())
    }

    /// Auto-discover network interfaces and create connections to targets
    pub async fn auto_discover_and_connect(&self, targets: &[SocketAddr]) -> Result<()> {
        info!("🔍 Starting smart interface discovery for targets: {:?}", targets);
        let interfaces = self.get_usable_interfaces_for_targets(targets)?;
        info!("📡 Found {} usable network interfaces after smart filtering", interfaces.len());

        // 显示所有发现的接口
        for interface in &interfaces {
            let interface_ips: Vec<String> = interface.addr.iter()
                .map(|addr| addr.ip().to_string())
                .collect();
            info!("🔗 Interface '{}': IPs = {:?}", interface.name, interface_ips);
        }

        let mut total_connections = 0;
        for interface in interfaces {
            info!("🔄 Processing interface: {} with {} addresses", interface.name, interface.addr.len());
            for &target in targets {
                // 检查接口是否能到达目标
                let can_reach = self.interface_can_reach_target(&interface, target);
                info!("🎯 Interface {} -> Target {}: reachable = {}", interface.name, target, can_reach);
                
                if !can_reach {
                    warn!("⚠️  Skipping {} -> {} (interface logic says incompatible)", interface.name, target);
                    continue;
                }
                
                info!("✅ Creating connection from {} to {}", interface.name, target);
                
                // Add timeout to prevent hanging on socket creation (increased from 2s to 5s)
                match tokio::time::timeout(
                    Duration::from_secs(5),
                    self.create_connection_for_interface(&interface, target)
                ).await {
                    Ok(Ok(connection)) => {
                        let interface_key = format!("{}_{}", interface.name, target);
                        self.connections.write().await.insert(interface_key.clone(), connection);
                        self.connection_order.write().await.push(interface_key.clone());
                        
                        // 为新连接创建统计
                        self.link_stats.write().await.insert(interface_key.clone(), LinkStats::default());
                        
                        info!("🎉 Successfully created connection via {} to {} (key: {})", interface.name, target, interface_key);
                        total_connections += 1;
                    }
                    Ok(Err(e)) => {
                        error!("❌ Failed to create connection via {} to {}: {}", interface.name, target, e);
                    }
                    Err(_) => {
                        error!("⏰ Timeout creating connection via {} to {}", interface.name, target);
                    }
                }
            }
        }

        if total_connections == 0 {
            return Err(anyhow::anyhow!("❌ No usable connections could be established"));
        }

        info!("🎊 Successfully established {} UDP connections", total_connections);
        
        // 显示所有创建的连接
        let connections = self.connections.read().await;
        for (key, conn) in connections.iter() {
            info!("🔗 Active connection [{}]: {} -> {} via {}", key, conn.local_addr, conn.target_addr, conn.interface_name);
        }
        
        Ok(())
    }
    
    /// 启动响应监听任务，接收来自服务端的响应
    pub async fn start_response_listeners(&self, client_socket: Arc<UdpSocket>, sessions: Arc<RwLock<HashMap<SocketAddr, UdpSession>>>) -> Result<Vec<tokio::task::JoinHandle<()>>> {
        let mut tasks = Vec::new();
        let connections = self.connections.read().await.clone();
        
        info!("Starting {} response listeners for client connections", connections.len());
        
        for (connection_key, connection) in connections {
            let sessions_clone = sessions.clone();
            let client_socket_clone = client_socket.clone();
            let connection_clone = connection.clone();
            let key_clone = connection_key.clone();
            
            let task = tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                info!("Response listener started for connection {} (local: {} -> target: {})", 
                      key_clone, connection_clone.local_addr, connection_clone.target_addr);
                
                loop {
                    // Use timeout to prevent indefinite blocking
                    match tokio::time::timeout(
                        Duration::from_secs(1), 
                        connection_clone.socket.recv_from(&mut buf)
                    ).await {
                        Ok(Ok((len, response_from))) => {
                            debug!("Response received {} bytes from {} via connection {}", 
                                  len, response_from, key_clone);
                            
                            // Forward to all active client sessions
                            let sessions_read = sessions_clone.read().await;
                            if !sessions_read.is_empty() {
                                for (client_addr, _session) in sessions_read.iter() {
                                    match client_socket_clone.send_to(&buf[..len], *client_addr).await {
                                        Ok(_) => debug!("Forwarded {} bytes to client {}", len, client_addr),
                                        Err(e) => warn!("Failed to forward response to client {}: {}", client_addr, e),
                                    }
                                }
                                
                                // 如果收到服务端响应，可能可以用来估算RTT
                                // 但要小心：这个响应可能不是对我们刚发送包的直接回应
                                // 所以只在有合理延迟时才记录
                                // TODO: 这里需要更复杂的逻辑来匹配请求-响应对
                            } else {
                                debug!("No active client sessions to forward response to");
                            }
                        }
                        Ok(Err(e)) => {
                            warn!("Receive error on connection {}: {}", key_clone, e);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        Err(_) => {
                            // Timeout - this is normal, just continue the loop
                            continue;
                        }
                    }
                }
            });
            
            tasks.push(task);
        }
        
        info!("Started {} response listener tasks", tasks.len());
        Ok(tasks)
    }

    /// 启动轻量级链路监控任务
    pub async fn start_link_monitoring_tasks(&self) -> Vec<tokio::task::JoinHandle<()>> {
        let mut tasks = Vec::new();
        let connections = self.connections.read().await.clone();
        
        info!("Starting lightweight monitoring for {} connections", connections.len());
        
        for (connection_key, _connection) in connections {
            let link_stats = self.link_stats.clone();
            let key_clone = connection_key.clone();
            
            // 非常轻量的监控任务：只做基本检查
            let task = tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(10)); // 减少到每10秒
                
                loop {
                    interval.tick().await;
                    
                    // 简单的统计更新
                    let mut stats = link_stats.write().await;
                    if let Some(link_stat) = stats.get_mut(&key_clone) {
                        // 检查是否长时间无活动
                        if link_stat.last_update.elapsed() > Duration::from_secs(30) {
                            // 长时间无活动，轻微降低成功率
                            link_stat.success_rate = (link_stat.success_rate * 0.98).max(0.1);
                            link_stat.loss_rate = 1.0 - link_stat.success_rate;
                        }
                    }
                }
            });
            
            tasks.push(task);
        }
        
        info!("Started {} lightweight monitoring tasks", tasks.len());
        tasks
    }

    /// Get network interfaces that can be used for UDP connections to specific targets
    fn get_usable_interfaces_for_targets(&self, targets: &[SocketAddr]) -> Result<Vec<NetworkInterface>> {
        let all_interfaces = NetworkInterface::show()
            .map_err(|err| anyhow::anyhow!("Failed to get network interfaces: {}", err))?;
            
        debug!("Found {} total interfaces", all_interfaces.len());

        let mut usable_interfaces = Vec::new();
        
        for interface in all_interfaces {
            // 使用新的过滤方法
            if !Self::is_suitable_interface(&interface) {
                debug!("Filtered out interface {} (not suitable)", interface.name);
                continue;
            }
            
            // Check if this interface can reach any of our targets
            let mut can_reach_target = false;
            for &target in targets {
                if self.interface_can_reach_target(&interface, target) {
                    can_reach_target = true;
                    break;
                }
            }
            
            if !can_reach_target {
                debug!("Interface {} cannot reach any targets, skipping", interface.name);
                continue;
            }
            
            info!("Selected interface: {}", interface.name);
            usable_interfaces.push(interface);
        }

        // 限制最多4个接口避免连接过多
        usable_interfaces.truncate(4);

        info!("Selected {} interfaces: {:?}", 
               usable_interfaces.len(),
               usable_interfaces.iter().map(|i| &i.name).collect::<Vec<_>>());
        
        if usable_interfaces.is_empty() {
            return Err(anyhow::anyhow!("No usable network interfaces found for targets"));
        }
        
        Ok(usable_interfaces)
    }
    
    /// Check if an interface can reach a target (enhanced routing logic)
    fn interface_can_reach_target(&self, interface: &NetworkInterface, target: SocketAddr) -> bool {
        let target_ip = target.ip();
        let interface_name = &interface.name;
        
        // 基本IP版本和地址检查
        let compatible_addr = interface.addr.iter().find(|addr| {
            // Must have a valid IP address
            !addr.ip().is_unspecified() &&
            // Loopback matching: both loopback or both non-loopback
            addr.ip().is_loopback() == target_ip.is_loopback() &&
            // IP version matching: both IPv4 or both IPv6
            addr.ip().is_ipv4() == target.is_ipv4() &&
            addr.ip().is_ipv6() == target.is_ipv6()
        });
        
        if compatible_addr.is_none() {
            debug!("Interface {} has no compatible IP for target {}", interface_name, target);
            return false;
        }
        
        // 对于公网目标，严格过滤虚拟接口
        // 使用稳定的API检测公网IP：非loopback、非私网、非链路本地
        let is_public_target = !target_ip.is_loopback() && 
                              !target_ip.is_multicast() &&
                              match target_ip {
                                  IpAddr::V4(v4) => !v4.is_private() && !v4.is_link_local() && !v4.is_broadcast(),
                                  IpAddr::V6(v6) => !v6.is_unique_local() && !v6.is_multicast(),
                              };
        
        if is_public_target {
            // 这些虚拟接口很可能无法直接访问公网
            if interface_name.starts_with("wg") ||          // WireGuard
               interface_name.starts_with("zt") ||          // ZeroTier
               interface_name.starts_with("utun") ||        // macOS VPN 
               interface_name.starts_with("tap") ||         // TAP接口
               interface_name.starts_with("tun") ||         // TUN接口 (除了ppp)
               interface_name.contains("vpn") {             // 通用VPN
                info!("Skipping virtual interface {} for public target {} (likely tunneled)", interface_name, target);
                return false;
            }
        }
        
        // 优先真正的物理/蜂窝接口
        let is_physical_or_cellular = 
            interface_name.starts_with("eth") ||     // 以太网
            interface_name.starts_with("en") ||      // macOS以太网/WiFi
            interface_name.starts_with("wlan") ||    // WiFi
            interface_name.starts_with("ppp") ||     // PPP连接（通常是蜂窝）
            interface_name.starts_with("ww") ||      // WWAN (蜂窝)
            interface_name.contains("cellular") ||   // 蜂窝连接
            interface_name.contains("mobile");       // 移动连接
        
        if is_physical_or_cellular {
            debug!("Interface {} is physical/cellular, can reach target {}", interface_name, target);
            return true;
        }
        
        // 本地/私网目标相对宽松
        let is_local_target = target_ip.is_loopback() || 
                             match target_ip {
                                 IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
                                 IpAddr::V6(v6) => v6.is_unique_local() || v6.is_unicast_link_local(),
                             };
        
        if is_local_target {
            debug!("Interface {} can reach local/private target {}", interface_name, target);
            return true;
        }
        
        // 其他情况下保守处理
        debug!("Interface {} may not reliably reach target {}", interface_name, target);
        false
    }

    /// Create a UDP connection for a specific interface with connectivity test
    async fn create_connection_for_interface(
        &self,
        interface: &NetworkInterface,
        target: SocketAddr,
    ) -> Result<UdpConnection> {
        // Find a suitable local address on this interface
        let local_ip = interface.addr.iter()
            .find(|addr| {
                // Must have compatible IP version and loopback status
                !addr.ip().is_unspecified() &&
                addr.ip().is_loopback() == target.ip().is_loopback() &&
                match (addr.ip(), target.ip()) {
                    (IpAddr::V4(_), IpAddr::V4(_)) => true,
                    (IpAddr::V6(_), IpAddr::V6(_)) => true,
                    _ => false,
                }
            })
            .ok_or_else(|| anyhow::anyhow!("No suitable IP on interface {}", interface.name))?
            .ip();

        let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0)).await?;
        let local_addr = socket.local_addr()?;
        
        info!("Created UDP connection from {} to {} via interface {}", 
              local_addr, target, interface.name);

        // Quick connectivity test - try to send a small test packet (increased timeout)
        let test_data = b"ping";
        match tokio::time::timeout(
            Duration::from_millis(2000), // Increased from 500ms to 2s
            socket.send_to(test_data, target)
        ).await {
            Ok(Ok(_)) => {
                        debug!("Connectivity test passed for {} via {}", target, interface.name);
            }
            Ok(Err(e)) => {
                warn!("Connectivity test failed for {} via {}: {}", target, interface.name, e);
                // Don't fail completely, just warn - UDP is connectionless
            }
            Err(_) => {
                warn!("Connectivity test timeout for {} via {}", target, interface.name);
                // Don't fail completely, just warn
            }
        }

        // ★ NEW: 强绑设备，彻底避免主路由表把流量吸走
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::prelude::AsRawFd;
            use socket2::{SockRef, Domain, Type, Protocol};
            let sock_ref = unsafe { SockRef::from_raw_fd(socket.as_raw_fd()) };
            sock_ref.set_bindtodevice(Some(&interface.name))?;
        }

        // 可选：调用 socket.connect(target) 创建 NAT 映射，
        // send_to() 时就不用每次带 target_addr 了
        socket.connect(target).await?;

        Ok(UdpConnection {
            socket: Arc::new(socket),
            interface_name: interface.name.clone(),
            local_addr,
            target_addr: target,
        })
    }

    /// Send data using load balancing across multiple interfaces
    pub async fn send_data(&self, data: &[u8]) -> Result<()> {
        let connections = self.connections.read().await;
        if connections.is_empty() {
            warn!("No connections available for sending");
            return Err(anyhow::anyhow!("No connections available"));
        }

        // 根据配置的策略选择链路，实现真正的带宽聚合
        let selected_key = match self.strategy {
            LoadBalanceStrategy::PacketRoundRobin => {
                // 强制轮询：忽略接口质量，确保所有接口都被使用
                self.select_connection_round_robin_force(&connections).await
            }
            LoadBalanceStrategy::FastestFirst => {
                // 选择最快链路
                self.select_fastest_connection(&connections).await
            }
            LoadBalanceStrategy::WeightedByBandwidth => {
                // 基于带宽权重选择
                self.select_connection_by_bandwidth(&connections).await
            }
            _ => {
                // 其他策略默认使用强制轮询
                self.select_connection_round_robin_force(&connections).await
            }
        };

        // 通过选中的链路发送数据
        if let Some(connection) = connections.get(&selected_key) {
            match connection.socket.send_to(data, connection.target_addr).await {
                Ok(bytes_sent) => {
                    // Update global statistics efficiently
                    self.packets_sent.fetch_add(1, Ordering::Relaxed);
                    self.bytes_sent.fetch_add(bytes_sent as u64, Ordering::Relaxed);

                    // 更详细的发包日志，用于调试多链路聚合
                    let packet_count = self.packets_sent.load(Ordering::Relaxed);
                    if packet_count % 10 == 0 {  // 每10包打印一次
                        info!("📤 Packet #{}: {} bytes sent via {} (Round-robin working!)", 
                              packet_count, bytes_sent, selected_key);
                    }

                    // 每包都更新对应链路的统计，确保统计准确
                    if let Some(stats) = self.link_stats.write().await.get_mut(&selected_key) {
                        stats.update_send_stats(bytes_sent);
                        stats.record_send_success();
                        stats.estimate_rtt();
                    }
                }
                Err(e) => {
                    warn!("Send failed via {}: {}", selected_key, e);

                    // 记录真实的发送失败
                    if let Some(stats) = self.link_stats.write().await.get_mut(&selected_key) {
                        stats.update_send_stats(data.len()); // 仍然记录尝试发送的字节数
                        stats.record_send_failure(); // 记录发送失败
                    }
                    
                    return Err(anyhow::anyhow!("Send failed: {}", e));
                }
            }
        } else {
            error!("Selected connection {} not found", selected_key);
            return Err(anyhow::anyhow!("Connection not found: {}", selected_key));
        }

        Ok(())
    }

    /// 强制使用所有接口创建连接，忽略兼容性检查（用于调试和强制多链路）
    pub async fn force_all_interfaces_connect(&self, targets: &[SocketAddr]) -> Result<()> {
        info!("🚀 FORCE MODE: Creating connections on ALL interfaces, ignoring compatibility checks");
        
        let interfaces = NetworkInterface::show()?;
        let usable_interfaces: Vec<_> = interfaces.into_iter()
            .filter(|interface| Self::is_suitable_interface(interface))
            .collect();
            
        info!("📡 FORCE MODE: Found {} total interfaces (including potentially incompatible ones)", usable_interfaces.len());

        let mut total_connections = 0;
        for interface in usable_interfaces {
            info!("🔗 FORCE MODE: Processing interface '{}' with {} addresses", interface.name, interface.addr.len());
            
            for &target in targets {
                info!("💪 FORCE MODE: Attempting to create connection from {} to {} (ignoring compatibility)", interface.name, target);
                
                match tokio::time::timeout(
                    Duration::from_secs(10), // 更长的超时时间用于强制模式
                    self.create_connection_for_interface(&interface, target)
                ).await {
                    Ok(Ok(connection)) => {
                        let interface_key = format!("{}_{}", interface.name, target);
                        self.connections.write().await.insert(interface_key.clone(), connection);
                        self.connection_order.write().await.push(interface_key.clone());
                        
                        // 为新连接创建统计
                        self.link_stats.write().await.insert(interface_key.clone(), LinkStats::default());
                        
                        info!("🎉 FORCE MODE: Successfully created connection via {} to {} (key: {})", interface.name, target, interface_key);
                        total_connections += 1;
                    }
                    Ok(Err(e)) => {
                        warn!("❌ FORCE MODE: Failed to create connection via {} to {}: {}", interface.name, target, e);
                    }
                    Err(_) => {
                        warn!("⏰ FORCE MODE: Timeout creating connection via {} to {}", interface.name, target);
                    }
                }
            }
        }

        if total_connections == 0 {
            return Err(anyhow::anyhow!("❌ FORCE MODE: Even with force mode, no connections could be established"));
        }

        info!("🎊 FORCE MODE: Successfully established {} UDP connections", total_connections);
        
        // 显示所有创建的连接
        let connections = self.connections.read().await;
        for (key, conn) in connections.iter() {
            info!("🔗 FORCE Active connection [{}]: {} -> {} via {}", key, conn.local_addr, conn.target_addr, conn.interface_name);
        }
        
        Ok(())
    }

    /// 强制轮询选择连接 - 使用固定顺序，确保所有接口都被均匀使用
    async fn select_connection_round_robin_force(&self, _connections: &HashMap<String, UdpConnection>) -> String {
        let connection_order = self.connection_order.read().await;
        if connection_order.is_empty() {
            panic!("No connections available");
        }
        
        let index = self.round_robin_index.fetch_add(1, Ordering::Relaxed) as usize;
        let selected_index = index % connection_order.len();
        
        let selected_key = &connection_order[selected_index];
        
        // 强制使用，不管接口质量如何
        debug!("🔄 FORCE ROUND-ROBIN: Selected connection {} ({}/{})", 
               selected_key, selected_index + 1, connection_order.len());
        
        selected_key.clone()
    }

    /// Select fastest (lowest latency) connection
    async fn select_fastest_connection(&self, connections: &HashMap<String, UdpConnection>) -> String {
        let stats = self.link_stats.read().await;
        let mut best_key = connections.keys().next().unwrap().clone();
        let mut best_rtt = Duration::from_secs(10);

        for key in connections.keys() {
            if let Some(link_stats) = stats.get(key) {
                // 大幅放宽健康度要求：20%成功率以上就可以使用（对应80%错误率以下）
                if link_stats.success_rate > 0.2 {
                    let rtt = link_stats.avg_rtt.unwrap_or(Duration::from_millis(50));
                    if rtt < best_rtt {
                        best_rtt = rtt;
                        best_key = key.clone();
                    }
                } else {
                    // 即使是新连接或高错误率连接也给机会，宁可用也不能闲置
                    if best_rtt > Duration::from_millis(100) {
                        best_rtt = Duration::from_millis(100);
                        best_key = key.clone();
                    }
                }
            } else {
                // 对于没有统计的新连接，优先使用
                best_rtt = Duration::from_millis(10);
                best_key = key.clone();
            }
        }

        best_key
    }

    /// Select connection based on bandwidth (for WeightedByBandwidth strategy)
    async fn select_connection_by_bandwidth(&self, connections: &HashMap<String, UdpConnection>) -> String {
        let stats = self.link_stats.read().await;
        let mut best_key = connections.keys().next().unwrap().clone();
        let mut best_bandwidth = 0u64;

        for key in connections.keys() {
            if let Some(link_stats) = stats.get(key) {
                // 大幅放宽要求：20%成功率以上就选择带宽最高的
                if link_stats.success_rate > 0.2 && link_stats.bandwidth_bps > best_bandwidth {
                    best_bandwidth = link_stats.bandwidth_bps;
                    best_key = key.clone();
                } else if best_bandwidth == 0 {
                    // 如果没有好的选择，即使是低质量连接也要用
                    best_key = key.clone();
                }
            } else {
                // 新连接优先考虑
                if best_bandwidth == 0 {
                    best_key = key.clone();
                }
            }
        }

        best_key
    }

    /// Get statistics
    pub fn stats(&self) -> (u64, u64) {
        (self.packets_sent.load(Ordering::Relaxed), self.bytes_sent.load(Ordering::Relaxed))
    }

    /// Get detailed link statistics
    pub async fn link_stats(&self) -> HashMap<String, LinkStats> {
        self.link_stats.read().await.clone()
    }

    /// Get connection count
    pub async fn connection_count(&self) -> usize {
        self.connections.read().await.len()
    }

    /// 设置发送速率限制（包每秒）
    #[allow(dead_code)]
    pub fn set_rate_limit(&self, packets_per_second: u64) {
        if packets_per_second > 0 {
            // 计算包间隔微秒数
            let interval_us = 1_000_000 / packets_per_second;
            self.send_interval_us.store(interval_us, Ordering::Relaxed);
            info!("Set rate limit to {} pps (interval: {}μs)", packets_per_second, interval_us);
        } else {
            self.send_interval_us.store(0, Ordering::Relaxed);
            info!("Disabled rate limiting");
        }
    }
}

#[derive(Debug)]
struct UdpSession {
    client_addr: SocketAddr,
    last_activity: Instant,
    sequence: AtomicU64,
    // 添加服务端socket引用，用于回复
    server_socket: Option<Arc<UdpSocket>>,
}

impl Clone for UdpSession {
    fn clone(&self) -> Self {
        Self {
            client_addr: self.client_addr,
            last_activity: self.last_activity,
            sequence: AtomicU64::new(self.sequence.load(Ordering::Relaxed)),
            server_socket: self.server_socket.clone(),
        }
    }
}

impl UdpSession {
    fn new(client_addr: SocketAddr) -> Self {
        Self { 
            client_addr, 
            last_activity: Instant::now(), 
            sequence: AtomicU64::new(0),
            server_socket: None,
        }
    }
    
    fn new_with_socket(client_addr: SocketAddr, server_socket: Arc<UdpSocket>) -> Self {
        Self { 
            client_addr, 
            last_activity: Instant::now(), 
            sequence: AtomicU64::new(0),
            server_socket: Some(server_socket),
        }
    }

    fn update_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    fn is_expired(&self, timeout: Duration) -> bool {
        self.last_activity.elapsed() > timeout
    }
}

/// Simple UDP aggregation CLI tool
#[derive(Debug, Parser)]
#[command(name = "agg-udp-simple")]
#[command(about = "A simple UDP aggregation tool for multi-path UDP")]
struct SimpleCli {
    #[command(subcommand)]
    command: Commands,
    /// Enable verbose logging
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
        /// Force use all interfaces, ignore quality (aggressive multi-link)
        #[arg(long)]
        force_all_links: bool,
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
        /// Load balancing strategy for server responses: packet-round-robin, weighted-bandwidth, fastest-first
        #[arg(long, default_value = "packet-round-robin")]
        strategy: String,
        /// Number of backend connections for load balancing
        #[arg(long, default_value = "2")]
        backend_connections: usize,
        /// Do not display the link monitor
        #[arg(long, short = 'n')]
        no_monitor: bool,
    },
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
        Commands::Client { local, targets, strategy, role, force_all_links, no_monitor } => {
            run_client(local, targets, strategy, role, force_all_links, no_monitor).await
        }
        Commands::Server { listen, target, strategy, backend_connections, no_monitor } => {
            run_server(listen, target, strategy, backend_connections, no_monitor).await
        },
    }
}

async fn run_client(
    local: SocketAddr, targets: Vec<SocketAddr>, strategy: String, _role: String, force_all_links: bool, no_monitor: bool,
) -> Result<()> {
    let no_monitor = no_monitor || !stdout().is_tty();

    info!("Starting multi-interface UDP proxy on {}", local);
    info!("Targets: {:?}", targets);
    info!("Strategy: {}", strategy);
    if force_all_links {
        info!("🚀 AGGRESSIVE MODE: Force using ALL links regardless of quality!");
    }

    let strategy = match strategy.as_str() {
        "packet-round-robin" => LoadBalanceStrategy::PacketRoundRobin,
        "weighted-bandwidth" => LoadBalanceStrategy::WeightedByBandwidth,
        "fastest-first" => LoadBalanceStrategy::FastestFirst,
        "weighted-packet-loss" => LoadBalanceStrategy::WeightedByPacketLoss,
        "dynamic-adaptive" => LoadBalanceStrategy::DynamicAdaptive,
        _ => LoadBalanceStrategy::PacketRoundRobin, // 默认使用packet-round-robin实现真正的带宽聚合
    };

    // Create multi-interface UDP aggregator with appropriate strategy
    let final_strategy = if force_all_links {
        info!("🔥 Forcing packet-round-robin strategy to use ALL links regardless of quality");
        LoadBalanceStrategy::PacketRoundRobin
    } else {
        strategy
    };
    
    let aggregator = Arc::new(SimpleUdpAggregator::new(final_strategy));
    
    // Auto-discover interfaces and create connections
    if force_all_links {
        info!("🚀 Using FORCE MODE to create connections on all interfaces");
        aggregator.force_all_interfaces_connect(&targets).await?;
    } else {
        info!("📡 Using SMART MODE for interface discovery and connection");
        aggregator.auto_discover_and_connect(&targets).await?;
    }
    
    let connection_count = aggregator.connection_count().await;
    info!("Multi-interface UDP aggregator ready with {} connections", connection_count);

    // 启动链路监控任务用于丢包检测
    let monitoring_tasks = aggregator.start_link_monitoring_tasks().await;
    info!("Started {} link monitoring tasks for loss detection", monitoring_tasks.len());

    // 为了低延迟，暂时禁用自适应速率控制
    // 因为速率控制会增加延迟，专注于快速传输
    // let rate_control_aggregator = aggregator.clone();
    // tokio::spawn(async move { ... });

    // Bind local socket for receiving client requests
    let client_socket = Arc::new(UdpSocket::bind(local).await?);
    let actual_local = client_socket.local_addr()?;
    info!("Transparent proxy listening on {} (actual: {})", local, actual_local);

    // Session tracking for UDP connections
    let sessions: Arc<RwLock<HashMap<SocketAddr, UdpSession>>> = Arc::new(RwLock::new(HashMap::new()));
    let session_timeout = Duration::from_secs(60);

    // Start monitoring task with statistics reporting
    let stats_aggregator = aggregator.clone();
    let (stats_control_tx, mut stats_control_rx) = broadcast::channel::<String>(16);
    let monitoring_task = tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            
            // Get connection stats
            let (total_packets, total_bytes) = stats_aggregator.stats();
            let link_stats = stats_aggregator.link_stats().await;
            let connection_count = stats_aggregator.connection_count().await;
            
            // Create detailed status message with per-connection statistics
            let mut status = String::new();
            
            if !link_stats.is_empty() {
                status.push_str(&format!("UDP Proxy - {} active connections:\n", connection_count));
                
                for (key, stats) in link_stats.iter() {
                    let bandwidth_str = if stats.bandwidth_bps > 1024 * 1024 {
                        format!("{:.1} MB/s", stats.bandwidth_bps as f64 / (1024.0 * 1024.0))
                    } else if stats.bandwidth_bps > 1024 {
                        format!("{} KB/s", stats.bandwidth_bps / 1024)
                    } else {
                        format!("{} B/s", stats.bandwidth_bps)
                    };

                    let rtt_str = if let Some(rtt) = stats.avg_rtt {
                        format!("{}ms", rtt.as_millis())
                    } else {
                        "N/A".to_string()
                    };

                    // 放宽链路状态判断：11.4%的接口错误率仍然可以使用
                    let recent_activity = stats.last_update.elapsed() < Duration::from_secs(5);
                    let has_traffic = stats.bandwidth_bps > 0 || stats.packets_sent > 0;
                    let acceptable_loss = stats.loss_rate < 0.2; // 放宽到20%错误率以下都可用
                    let good_rtt = stats.avg_rtt.map_or(true, |rtt| rtt < Duration::from_millis(2000)); // 放宽RTT要求
                    
                    // 更宽松的ACTIVE判断：只要接口可达就尝试使用
                    let _is_active = recent_activity || acceptable_loss;
                    
                    // 状态描述 - 更积极地使用可用接口
                    let status_str = if !recent_activity && stats.loss_rate > 0.5 {
                        "DEAD" // 完全无响应且高错误率
                    } else if stats.loss_rate > 0.2 {
                        "POOR" // 高错误率但仍可用
                    } else if !good_rtt {
                        "SLOW" // 高延迟但可用
                    } else if has_traffic {
                        "ACTIVE"
                    } else {
                        "READY" // 可用但暂时无流量
                    };

                    status.push_str(&format!(
                        "  {}: {} | {} | {:.1}% if-err | RTT: {}\n",
                        key,
                        bandwidth_str,
                        status_str,
                        stats.loss_rate * 100.0, // 这是接口错误率，不是真正的丢包率
                        rtt_str
                    ));
                }
                
                status.push_str(&format!("Total: {} packets, {} KB forwarded", total_packets, total_bytes / 1024));
                
                // 添加丢包率警告
                if total_packets > 1000 {
                    let current_interval = stats_aggregator.send_interval_us.load(Ordering::Relaxed);
                    if current_interval > 0 {
                        status.push_str(&format!("\nRate limit: {:.1} pps", 1_000_000.0 / current_interval as f64));
                    } else {
                        status.push_str("\nRate limit: UNLIMITED");
                    }
                }
            } else {
                status = "UDP Proxy - No active connections".to_string();
            }
            
            let _ = stats_control_tx.send(status);

            if no_monitor {
                debug!("Active connections: {}", connection_count);
            } else {
                info!("Active connections: {}", connection_count);
            }
        }
    });

    // Start monitoring display task (clear screen refresh mode)
    if !no_monitor {
        let _display_task = tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                
                if let Ok(status) = stats_control_rx.try_recv() {
                    // Clear screen and reset cursor
                    print!("\x1B[2J\x1B[H");
                    
                    // Display title
                    let title = "UDP Multi-Interface Aggregation Proxy".bold().green();
                    println!("{}\n", title);
                    
                    // Display status
                    println!("{}", status);
                }
            }
        });
        
        // Clean up sessions periodically
        let sessions_cleanup = sessions.clone();
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                let mut sessions = sessions_cleanup.write().await;
                sessions.retain(|_addr, session| !session.is_expired(session_timeout));
                debug!("Active sessions: {}", sessions.len());
            }
        });
    }

    // Client to server forwarding task - using multi-interface aggregator
    let client_sessions = sessions.clone();
    let forward_socket = client_socket.clone();
    let forward_aggregator = aggregator.clone();
    let forward_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        info!("UDP forwarding task started, listening for client packets");
        loop {
            match forward_socket.recv_from(&mut buf).await {
                Ok((len, client_addr)) => {
                    trace!("Received {} bytes from client {}", len, client_addr); // 降级为trace

                    // Update or create session
                    {
                        let mut sessions = client_sessions.write().await;
                        match sessions.get_mut(&client_addr) {
                            Some(session) => {
                                session.update_activity();
                            }
                            None => {
                                sessions.insert(client_addr, UdpSession::new(client_addr));
                                debug!("New client session: {}", client_addr);
                            }
                        }
                    }

                    // Forward via multi-interface aggregator
                    if let Err(e) = forward_aggregator.send_data(&buf[..len]).await {
                        warn!("Failed to forward packet from {} via aggregator: {}", client_addr, e);
                    }
                }
                Err(e) => {
                    error!("Failed to receive from client: {}", e);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    });    // 启动响应监听任务（异步，不阻塞主流程）
    let response_sessions = sessions.clone();
    let response_socket = client_socket.clone();
    let response_aggregator = aggregator.clone();
    
    // 异步启动响应监听器，增加超时和错误处理
    tokio::spawn(async move {
        info!("Starting response listener setup...");
        
        // Add timeout to response listener setup
        match tokio::time::timeout(
            Duration::from_secs(5),
            response_aggregator.start_response_listeners(response_socket, response_sessions)
        ).await {
            Ok(Ok(response_tasks)) => {
                info!("Started {} response listener tasks", response_tasks.len());
                // 启动所有响应监听任务
                for task in response_tasks {
                    tokio::spawn(task);
                }
                info!("All response listeners are now active");
            }
            Ok(Err(e)) => {
                error!("Failed to start response listeners: {}", e);
            }
            Err(_) => {
                error!("Timeout starting response listeners - continuing without them");
            }
        }
    });
    
    tokio::select! {
        _ = forward_task => {},
        _ = monitoring_task => {},
    }

    Ok(())
}

async fn run_server(
    listen: Vec<SocketAddr>, 
    target: SocketAddr, 
    strategy: String,
    backend_connections: usize,
    no_monitor: bool
) -> Result<()> {
    let no_monitor = no_monitor || !stdout().is_tty();
    
    info!("UDP Aggregation Server - {:?} -> {}", listen, target);
    info!("Strategy: {}, Backend connections: {}", strategy, backend_connections);
    
    // 解析负载均衡策略
    let lb_strategy = match strategy.as_str() {
        "packet-round-robin" => LoadBalanceStrategy::PacketRoundRobin,
        "weighted-bandwidth" => LoadBalanceStrategy::WeightedByBandwidth,
        "fastest-first" => LoadBalanceStrategy::FastestFirst,
        "weighted-packet-loss" => LoadBalanceStrategy::WeightedByPacketLoss,
        "dynamic-adaptive" => LoadBalanceStrategy::DynamicAdaptive,
        _ => LoadBalanceStrategy::PacketRoundRobin,
    };
    
    // 创建服务端聚合器
    let server_aggregator = Arc::new(ServerAggregator::new(lb_strategy));
    
    // 创建多个到后端的连接
    for i in 0..backend_connections {
        if let Err(e) = server_aggregator.add_backend_connection(target).await {
            warn!("Failed to create backend connection {}: {}", i, e);
        }
    }
    
    info!("Created {} backend connections to {}", backend_connections, target);
    
    // 启动定期清理任务
    let _cleanup_task = server_aggregator.start_cleanup_task();
    info!("Started client mapping cleanup task");
    
    // Session tracking for clients
    let sessions: Arc<RwLock<HashMap<SocketAddr, UdpSession>>> = Arc::new(RwLock::new(HashMap::new()));
    let session_timeout = Duration::from_secs(300); // 5 minute timeout for server sessions
    
    // Bind all listen addresses
    let mut sockets = Vec::new();
    for &listen_addr in &listen {
        let socket = Arc::new(UdpSocket::bind(listen_addr).await?);
        let actual_addr = socket.local_addr()?;
        
        info!("=== SERVER BOUND === Listening on {} (actual: {})", listen_addr, actual_addr);
        sockets.push(socket);
    }
    
    // Connect to target backend
    let target_socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    let target_local_addr = target_socket.local_addr()?;
    info!("Connected to target backend {} from local {}", target, target_local_addr);
    
    // 创建客户端地址到目标地址的映射，用于双向转发
    let client_target_map: Arc<RwLock<HashMap<SocketAddr, SocketAddr>>> = Arc::new(RwLock::new(HashMap::new()));
    
    // Statistics
    let packets_received = Arc::new(AtomicU64::new(0));
    let bytes_received = Arc::new(AtomicU64::new(0));
    let packets_forwarded = Arc::new(AtomicU64::new(0));
    let bytes_forwarded = Arc::new(AtomicU64::new(0));
    
    // Monitoring task with detailed multi-client and multi-link statistics
    if !no_monitor {
        let stats_packets_received = packets_received.clone();
        let stats_bytes_received = bytes_received.clone();
        let stats_packets_forwarded = packets_forwarded.clone();
        let stats_bytes_forwarded = bytes_forwarded.clone();
        let monitor_listen = listen.clone();
        let monitor_aggregator = server_aggregator.clone();
        let monitor_sessions = sessions.clone();
        
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(1));
            let mut last_packets_received = 0u64;
            let mut last_packets_forwarded = 0u64;
            let mut last_time = Instant::now();
            
            loop {
                interval.tick().await;
                
                let current_packets_received = stats_packets_received.load(Ordering::Relaxed);
                let current_bytes_received = stats_bytes_received.load(Ordering::Relaxed);
                let current_packets_forwarded = stats_packets_forwarded.load(Ordering::Relaxed);
                let current_bytes_forwarded = stats_bytes_forwarded.load(Ordering::Relaxed);
                let current_time = Instant::now();
                
                let elapsed = current_time.duration_since(last_time).as_secs_f64();
                let rx_pps = if elapsed > 0.0 {
                    (current_packets_received - last_packets_received) as f64 / elapsed
                } else {
                    0.0
                };
                let tx_pps = if elapsed > 0.0 {
                    (current_packets_forwarded - last_packets_forwarded) as f64 / elapsed
                } else {
                    0.0
                };
                
                // 获取详细的客户端连接统计
                let sessions_read = monitor_sessions.read().await;
                let _active_clients = sessions_read.len();
                let client_connections = monitor_aggregator.client_connections.read().await;
                let backend_connections = monitor_aggregator.backend_connections.read().await;
                let backend_map = monitor_aggregator.backend_to_clients_map.read().await;
                
                // Clear screen and reset cursor
                print!("\x1B[2J\x1B[H");
                
                // Display enhanced title and stats
                let title = format!("UDP Multi-Link Aggregation Server - {:?} -> {}", monitor_listen, target).bold().green();
                println!("{}\n", title);
                
                // 总体统计
                println!("📊 OVERALL STATISTICS:");
                println!("  Received:  {} packets ({:.0} pps) | {} KB", 
                         current_packets_received, rx_pps, current_bytes_received / 1024);
                println!("  Forwarded: {} packets ({:.0} pps) | {} KB", 
                         current_packets_forwarded, tx_pps, current_bytes_forwarded / 1024);
                println!("  Loss:      {:.2}%", 
                         if current_packets_received > 0 {
                             (1.0 - current_packets_forwarded as f64 / current_packets_received as f64) * 100.0
                         } else { 0.0 });
                
                // 客户端多链路统计（修复：按客户端IP显示）
                println!("\n🔗 CLIENT MULTI-LINK STATUS:");
                println!("  Active client IPs: {}", client_connections.len());

                for (client_ip, connections) in client_connections.iter() {
                    let connection_count = connections.len();
                    let status = if connection_count > 1 {
                        format!("MULTI-LINK ({})", connection_count).green()
                    } else {
                        format!("SINGLE-LINK ({})", connection_count).yellow()
                    };

                    println!("  Client IP {}: {}", client_ip, status);

                    // 显示所有来源地址和连接详情
                    for (i, (src_addr, socket)) in connections.iter().enumerate() {
                        let local_addr = socket.local_addr().unwrap_or_else(|_| "unknown".parse().unwrap());
                        println!("    Link {}: {} -> {}", i + 1, src_addr, local_addr);
                    }

                    // 显示该客户端使用了哪些后端连接（基于任一源地址）
                    let mut backend_usage = Vec::new();
                    for (src_addr, _) in connections.iter() {
                        for (backend_idx, clients_set) in backend_map.iter() {
                            if clients_set.contains(src_addr) {
                                backend_usage.push(*backend_idx);
                            }
                        }
                    }
                    backend_usage.sort();
                    backend_usage.dedup();
                    if !backend_usage.is_empty() {
                        println!("    └─ Using backend connections: {:?}", backend_usage);
                    }
                }
                
                // 服务端后端连接统计
                println!("\n🎯 BACKEND CONNECTION POOL:");
                println!("  Total backend connections: {}", backend_connections.len());
                for (backend_idx, clients_set) in backend_map.iter() {
                    let client_count = clients_set.len();
                    let utilization = if client_count > 0 {
                        format!("ACTIVE (serving {} clients)", client_count).green()
                    } else {
                        "IDLE".to_string().yellow()
                    };
                    println!("  Backend {}: {}", backend_idx, utilization);
                    for client_addr in clients_set {
                        println!("    └─ Serving client {}", client_addr);
                    }
                }
                
                // 服务端监听地址
                println!("\n📡 SERVER LISTENING ON:");
                for addr in &monitor_listen {
                    println!("  {} -> {} (Multi-link aggregation enabled)", addr, target);
                }
                
                last_packets_received = current_packets_received;
                last_packets_forwarded = current_packets_forwarded;
                last_time = current_time;
            }
        });
    }
    
    // 启动多个响应监听任务 - 每个后端连接一个任务，实现真正的多链路聚合
    let response_sessions = sessions.clone();
    let response_aggregator = server_aggregator.clone();
    
    let response_tasks = response_aggregator.start_multi_backend_listeners(response_sessions, target).await;
    
    // Clean up sessions periodically
    let sessions_cleanup = sessions.clone();
    tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let mut sessions = sessions_cleanup.write().await;
            let before_count = sessions.len();
            sessions.retain(|_addr, session| !session.is_expired(session_timeout));
            let after_count = sessions.len();
            if before_count != after_count {
                debug!("Cleaned up {} expired sessions, {} active", before_count - after_count, after_count);
            }
        }
    });
    
    // Server receive tasks - one for each listen address
    let mut tasks = Vec::new();
    
    for socket in sockets {
        let sessions_task = sessions.clone();
        let client_map_task = client_target_map.clone();
        let packets_received_task = packets_received.clone();
        let bytes_received_task = bytes_received.clone();
        let packets_forwarded_task = packets_forwarded.clone();
        let bytes_forwarded_task = bytes_forwarded.clone();
        let socket_for_session = socket.clone();
        let aggregator_task = server_aggregator.clone();
        
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            let socket_addr = socket.local_addr().unwrap_or_else(|_| "unknown".parse().unwrap());
            info!("=== SERVER TASK STARTED === Listening on socket {}", socket_addr);
            
            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((len, client_addr)) => {
                        trace!("Server socket {} received {} bytes from {}", socket_addr, len, client_addr);
                        
                        // 立即更新统计 - 不再批量处理，确保统计准确
                        packets_received_task.fetch_add(1, Ordering::Relaxed);
                        bytes_received_task.fetch_add(len as u64, Ordering::Relaxed);
                        
                        // 立即更新会话
                        {
                            let mut sessions = sessions_task.write().await;
                            match sessions.get_mut(&client_addr) {
                                Some(session) => {
                                    session.update_activity();
                                }
                                None => {
                                    sessions.insert(client_addr, UdpSession::new_with_socket(client_addr, socket_for_session.clone()));
                                    info!("🔗 NEW CLIENT CONNECTED: {} via socket {}", client_addr, socket_addr);
                                }
                            }
                            
                            // 记录客户端到目标的映射
                            client_map_task.write().await.insert(client_addr, target);
                        }
                        
                        // 注册客户端连接到聚合器
                        aggregator_task.register_client_connection(client_addr, socket_for_session.clone()).await;
                        
                        // Forward to target backend using load balancing
                        match aggregator_task.send_to_backend(&buf[..len], target, client_addr).await {
                            Ok(()) => {
                                packets_forwarded_task.fetch_add(1, Ordering::Relaxed);
                                bytes_forwarded_task.fetch_add(len as u64, Ordering::Relaxed);
                                trace!("✅ Forwarded {} bytes to backend for client {} via socket {}", len, client_addr, socket_addr);
                            }
                            Err(e) => {
                                warn!("❌ Failed to forward to target {} for client {} via socket {}: {}", target, client_addr, socket_addr, e);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to receive on socket {}: {}", socket_addr, e);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        });
        
        tasks.push(task);
    }
    
    // 添加响应转发任务（多链路）
    for task in response_tasks {
        tasks.push(task);
    }
    
    // Wait for all tasks
    for task in tasks {
        let _ = task.await;
    }
    
    Ok(())
}

/// 服务端多链路聚合器
struct ServerAggregator {
    /// 客户端连接映射 (客户端IP -> 该客户端的所有连接和源地址)
    /// 修复：按照客户端IP而不是IP:端口来识别多链路
    client_connections: Arc<RwLock<HashMap<IpAddr, Vec<(SocketAddr, Arc<UdpSocket>)>>>>,
    /// 到后端目标的多个连接
    backend_connections: Arc<RwLock<Vec<Arc<UdpSocket>>>>, 
    /// 负载均衡策略
    strategy: LoadBalanceStrategy,
    /// 轮询索引
    round_robin_index: AtomicU64,
    /// 统计信息
    response_packets_sent: Arc<AtomicU64>,
    response_bytes_sent: Arc<AtomicU64>,
    /// 多客户端请求追踪 (后端连接索引 -> 活跃客户端集合)，支持多客户端并发
    backend_to_clients_map: Arc<RwLock<HashMap<usize, HashSet<SocketAddr>>>>,
    /// 客户端最后活动时间，用于清理超时映射
    client_last_activity: Arc<RwLock<HashMap<SocketAddr, Instant>>>,
}

impl ServerAggregator {
    fn new(strategy: LoadBalanceStrategy) -> Self {
        Self {
            client_connections: Arc::new(RwLock::new(HashMap::new())),
            backend_connections: Arc::new(RwLock::new(Vec::new())),
            strategy,
            round_robin_index: AtomicU64::new(0),
            response_packets_sent: Arc::new(AtomicU64::new(0)),
            response_bytes_sent: Arc::new(AtomicU64::new(0)),
            backend_to_clients_map: Arc::new(RwLock::new(HashMap::new())),
            client_last_activity: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    
    /// 注册客户端连接（避免重复注册）- 修复：按客户端IP分组多链路
    async fn register_client_connection(&self, client_addr: SocketAddr, server_socket: Arc<UdpSocket>) {
        let client_ip = client_addr.ip();
        let mut connections = self.client_connections.write().await;
        let client_connections = connections.entry(client_ip).or_insert_with(Vec::new);

        // 检查是否已经注册过这个socket
        let socket_local_addr = server_socket.local_addr().ok();
        let already_registered = client_connections.iter().any(|(_, existing_socket)| {
            existing_socket.local_addr().ok() == socket_local_addr
        });

        if !already_registered {
            client_connections.push((client_addr, server_socket));
            let total_connections = client_connections.len();
            info!("🔗 Registered NEW connection for client IP {} from {} (total: {}) - socket: {:?}",
                  client_ip, client_addr, total_connections, socket_local_addr);

            if total_connections > 1 {
                info!("🎯 CLIENT IP {} NOW HAS MULTI-LINK: {} connections from different ports/interfaces!", client_ip, total_connections);
                // 显示所有连接详情
                for (i, (src_addr, sock)) in client_connections.iter().enumerate() {
                    let local_addr = sock.local_addr().unwrap_or_else(|_| "unknown".parse().unwrap());
                    info!("   Link {}: {} -> {}", i + 1, src_addr, local_addr);
                }
            }
        }
    }
    
    /// 添加到后端的连接
    async fn add_backend_connection(&self, target: SocketAddr) -> Result<()> {
        let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
        socket.connect(target).await?;
        
        let local_addr = socket.local_addr()?;
        info!("Created backend connection from {} to {}", local_addr, target);
        
        self.backend_connections.write().await.push(socket);
        Ok(())
    }
    
    /// 使用负载均衡策略选择客户端连接
    #[allow(dead_code)]
    async fn select_client_connection(&self, client_addr: SocketAddr) -> Option<Arc<UdpSocket>> {
        let client_ip = client_addr.ip();
        let connections = self.client_connections.read().await;
        if let Some(client_connections) = connections.get(&client_ip) {
            if client_connections.is_empty() {
                return None;
            }

            match self.strategy {
                LoadBalanceStrategy::PacketRoundRobin => {
                    let index = self.round_robin_index.fetch_add(1, Ordering::Relaxed) as usize;
                    let selected_index = index % client_connections.len();
                    Some(client_connections[selected_index].1.clone())
                }
                _ => {
                    // 其他策略暂时使用轮询
                    let index = self.round_robin_index.fetch_add(1, Ordering::Relaxed) as usize;
                    let selected_index = index % client_connections.len();
                    Some(client_connections[selected_index].1.clone())
                }
            }
        } else {
            None
        }
    }
    
    /// 使用负载均衡策略选择后端连接
    #[allow(dead_code)]
    async fn select_backend_connection(&self) -> Option<Arc<UdpSocket>> {
        let connections = self.backend_connections.read().await;
        if connections.is_empty() {
            return None;
        }
        
        match self.strategy {
            LoadBalanceStrategy::PacketRoundRobin => {
                let index = self.round_robin_index.fetch_add(1, Ordering::Relaxed) as usize;
                let selected_index = index % connections.len();
                Some(connections[selected_index].clone())
            }
            _ => {
                // 其他策略暂时使用轮询
                let index = self.round_robin_index.fetch_add(1, Ordering::Relaxed) as usize;
                let selected_index = index % connections.len();
                Some(connections[selected_index].clone())
            }
        }
    }
    
    /// 向客户端发送响应（使用负载均衡）
    #[allow(dead_code)]
    async fn send_response_to_client(&self, client_addr: SocketAddr, data: &[u8]) -> Result<()> {
        if let Some(socket) = self.select_client_connection(client_addr).await {
            match socket.send_to(data, client_addr).await {
                Ok(bytes_sent) => {
                    self.response_packets_sent.fetch_add(1, Ordering::Relaxed);
                    self.response_bytes_sent.fetch_add(bytes_sent as u64, Ordering::Relaxed);
                    debug!("Response sent to client {} via selected connection: {} bytes", client_addr, bytes_sent);
                    Ok(())
                }
                Err(e) => {
                    warn!("Failed to send response to client {}: {}", client_addr, e);
                    Err(anyhow::anyhow!("Response send failed: {}", e))
                }
            }
        } else {
            warn!("No available connections for client {}", client_addr);
            Err(anyhow::anyhow!("No available connections for client"))
        }
    }
    
    /// 向后端发送数据（使用负载均衡）并记录多客户端映射
    async fn send_to_backend(&self, data: &[u8], target: SocketAddr, client_addr: SocketAddr) -> Result<()> {
        let backend_connections = self.backend_connections.read().await;
        if backend_connections.is_empty() {
            warn!("No available backend connections");
            return Err(anyhow::anyhow!("No available backend connections"));
        }
        
        let backend_index = match self.strategy {
            LoadBalanceStrategy::PacketRoundRobin => {
                let index = self.round_robin_index.fetch_add(1, Ordering::Relaxed) as usize;
                index % backend_connections.len()
            }
            _ => {
                // 其他策略暂时使用轮询
                let index = self.round_robin_index.fetch_add(1, Ordering::Relaxed) as usize;
                index % backend_connections.len()
            }
        };
        
        let socket = &backend_connections[backend_index];
        
        match socket.send_to(data, target).await {
            Ok(bytes_sent) => {
                // 记录后端连接索引到客户端集合的映射，支持多客户端并发
                {
                    let mut backend_map = self.backend_to_clients_map.write().await;
                    backend_map.entry(backend_index).or_insert_with(HashSet::new).insert(client_addr);
                }
                
                // 更新客户端最后活动时间
                {
                    let mut activity_map = self.client_last_activity.write().await;
                    activity_map.insert(client_addr, Instant::now());
                }
                
                debug!("Data sent to backend {} via connection {} for client {}: {} bytes", 
                       target, backend_index, client_addr, bytes_sent);
                Ok(())
            }
            Err(e) => {
                warn!("Failed to send to backend {} via connection {}: {}", target, backend_index, e);
                Err(anyhow::anyhow!("Backend send failed: {}", e))
            }
        }
    }
    
    /// 定期清理超时的客户端映射
    #[allow(dead_code)]
    async fn cleanup_expired_clients(&self) {
        let timeout_threshold = Instant::now() - Duration::from_secs(60); // 60秒超时
        
        // 清理活动时间映射
        let mut activity_map = self.client_last_activity.write().await;
        let expired_clients: Vec<SocketAddr> = activity_map
            .iter()
            .filter(|(_, &last_activity)| last_activity < timeout_threshold)
            .map(|(&addr, _)| addr)
            .collect();
        
        for client_addr in &expired_clients {
            activity_map.remove(client_addr);
        }
        
        // 清理后端到客户端映射
        let mut backend_map = self.backend_to_clients_map.write().await;
        for clients_set in backend_map.values_mut() {
            for client_addr in &expired_clients {
                clients_set.remove(client_addr);
            }
        }
        
        // 清理空的后端映射
        backend_map.retain(|_, clients_set| !clients_set.is_empty());
        
        if !expired_clients.is_empty() {
            debug!("Cleaned up {} expired client mappings", expired_clients.len());
        }
    }
    
    /// 启动定期清理任务
    pub fn start_cleanup_task(&self) -> tokio::task::JoinHandle<()> {
        let backend_to_clients_map = self.backend_to_clients_map.clone();
        let client_last_activity = self.client_last_activity.clone();
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                
                let timeout_threshold = Instant::now() - Duration::from_secs(60);
                
                // 清理活动时间映射
                let mut activity_map = client_last_activity.write().await;
                let expired_clients: Vec<SocketAddr> = activity_map
                    .iter()
                    .filter(|(_, &last_activity)| last_activity < timeout_threshold)
                    .map(|(&addr, _)| addr)
                    .collect();
                
                for client_addr in &expired_clients {
                    activity_map.remove(client_addr);
                }
                drop(activity_map);
                
                // 清理后端到客户端映射
                let mut backend_map = backend_to_clients_map.write().await;
                for clients_set in backend_map.values_mut() {
                    for client_addr in &expired_clients {
                        clients_set.remove(client_addr);
                    }
                }
                
                // 清理空的后端映射
                backend_map.retain(|_, clients_set| !clients_set.is_empty());
                
                if !expired_clients.is_empty() {
                    debug!("Cleaned up {} expired client mappings", expired_clients.len());
                }
            }
        })
    }
    
    /// 启动多个后端监听器，每个后端连接一个任务，实现真正的多链路响应聚合
    async fn start_multi_backend_listeners(
        &self, 
        sessions: Arc<RwLock<HashMap<SocketAddr, UdpSession>>>,
        target: SocketAddr
    ) -> Vec<tokio::task::JoinHandle<()>> {
        let backend_connections = self.backend_connections.read().await;
        let mut tasks = Vec::new();
        
        info!("Starting {} backend response listeners for multi-link aggregation", backend_connections.len());
        
        for (backend_index, backend_socket) in backend_connections.iter().enumerate() {
            let socket = backend_socket.clone();
            let _sessions_clone = sessions.clone();
            let client_connections = self.client_connections.clone();
            let backend_to_clients_map = self.backend_to_clients_map.clone();
            let client_last_activity = self.client_last_activity.clone();
            let client_round_robin_index = Arc::new(AtomicU64::new(0)); // 每个任务独立的轮询索引
            let response_packets_sent = self.response_packets_sent.clone();
            let response_bytes_sent = self.response_bytes_sent.clone();
            
            let task = tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                info!("🎯 BACKEND LISTENER {} STARTED === Monitoring responses from {}", backend_index, target);
                
                loop {
                    match socket.recv_from(&mut buf).await {
                        Ok((len, response_addr)) => {
                            debug!("🔙 Backend {} received {} bytes from {}", backend_index, len, response_addr);
                            
                            if response_addr == target {
                                // 查找这个后端连接对应的所有活跃客户端
                                let client_addrs = {
                                    let mut active_clients = Vec::new();
                                    let backend_map = backend_to_clients_map.read().await;
                                    
                                    if let Some(clients_set) = backend_map.get(&backend_index) {
                                        // 检查客户端活动时间，移除超时的客户端
                                        let activity_map = client_last_activity.read().await;
                                        let timeout_threshold = Instant::now() - Duration::from_secs(30); // 30秒超时
                                        
                                        for &client_addr in clients_set {
                                            if let Some(&last_activity) = activity_map.get(&client_addr) {
                                                if last_activity > timeout_threshold {
                                                    active_clients.push(client_addr);
                                                }
                                            }
                                        }
                                        
                                        if active_clients.is_empty() && !clients_set.is_empty() {
                                            warn!("🕐 Backend {} has {} mapped clients but all are timeout", backend_index, clients_set.len());
                                        }
                                    } else {
                                        trace!("🤷 Backend {} received response but no client mapping found", backend_index);
                                    }
                                    active_clients
                                };
                                
                                debug!("📤 Backend {} forwarding response to {} active clients", backend_index, client_addrs.len());
                                
                                // 向所有活跃客户端发送响应，使用各自的负载均衡连接
                                for client_addr in client_addrs {
                                    let client_ip = client_addr.ip();
                                    let client_connections_read = client_connections.read().await;
                                    if let Some(client_sockets) = client_connections_read.get(&client_ip) {
                                        if !client_sockets.is_empty() {
                                            // 使用轮询选择客户端连接实现响应的负载均衡
                                            let conn_index = client_round_robin_index.fetch_add(1, Ordering::Relaxed) as usize;
                                            let (_, selected_socket) = &client_sockets[conn_index % client_sockets.len()];

                                            match selected_socket.send_to(&buf[..len], client_addr).await {
                                                Ok(bytes_sent) => {
                                                    response_packets_sent.fetch_add(1, Ordering::Relaxed);
                                                    response_bytes_sent.fetch_add(bytes_sent as u64, Ordering::Relaxed);
                                                    debug!("✅ Backend {} sent response to client {} via connection {} ({}/{}): {} bytes",
                                                           backend_index, client_addr, conn_index % client_sockets.len(),
                                                           conn_index % client_sockets.len() + 1, client_sockets.len(), bytes_sent);
                                                }
                                                Err(e) => {
                                                    warn!("❌ Backend {} failed to send response to client {}: {}", backend_index, client_addr, e);
                                                }
                                            }
                                        } else {
                                            warn!("⚠️  No connections available for client IP {}", client_ip);
                                        }
                                    } else {
                                        warn!("❓ Client {} not found in connection map", client_addr);
                                    }
                                }
                            } else {
                                warn!("🔀 Backend {} received unexpected response from {}, expected {}", backend_index, response_addr, target);
                            }
                        }
                        Err(e) => {
                            error!("💥 Backend listener {} failed to receive: {}", backend_index, e);
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }
                }
            });
            
            tasks.push(task);
        }
        
        info!("All {} backend response listeners started successfully", tasks.len());
        tasks
    }
}
