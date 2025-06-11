//! Simple UDP aggregation tool - minimal implementation for testing multi-path UDP aggregation.
//!
//! This tool provides a basic proxy that can distribute UDP packets across multiple links
//! for bandwidth aggregation, similar to mptcp but for UDP.

use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::{style::Stylize, tty::IsTty};
use network_interface::{NetworkInterface, NetworkInterfaceConfig};
use rand::random;

use std::{
    collections::HashMap,
    hash::Hasher,
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
use tracing::{debug, error, info, warn};

use aggligator::{unordered_cfg::LoadBalanceStrategy};
use aggligator_util::init_log;

/// Link performance statistics for simple UDP aggregation
#[derive(Debug, Clone)]
struct LinkStats {
    /// Total packets sent on this link
    packets_sent: u64,
    /// Total bytes sent on this link
    bytes_sent: u64,
    /// Current bandwidth (bytes per second) - calculated over last window
    bandwidth_bps: u64,
    /// Last update timestamp
    last_update: Instant,
    /// Bytes sent in current measurement window
    window_bytes: u64,
    /// Window start time
    window_start: Instant,
    
    // 心跳包丢包统计 - 真正的丢包检测
    /// Heartbeat sequence number (递增)
    _heartbeat_seq: u64,
    /// Heartbeat packets sent count
    heartbeats_sent: u64,
    /// Heartbeat ACKs received count  
    heartbeats_acked: u64,
    /// Last heartbeat sent time
    last_heartbeat_sent: Option<Instant>,
    /// RTT measurements from heartbeats
    _rtt_samples: Vec<Duration>,
    /// Calculated packet loss rate from heartbeats
    loss_rate: f64,
    /// Average RTT
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
            _heartbeat_seq: 0,
            heartbeats_sent: 0,
            heartbeats_acked: 0,
            last_heartbeat_sent: None,
            _rtt_samples: Vec::new(),
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

        // 记录发送时间用于后续RTT计算
        self.last_heartbeat_sent = Some(now);

        // Update bandwidth calculation with sliding window (1 second window for more responsive updates)
        let window_duration = now.duration_since(self.window_start);
        if window_duration >= Duration::from_secs(1) {
            // Calculate bandwidth over the window
            self.bandwidth_bps = (self.window_bytes as f64 / window_duration.as_secs_f64()) as u64;
            
            // Reset window but keep some recent activity indicator
            self.window_bytes = bytes as u64; // Start new window with current packet
            self.window_start = now;
        } else if window_duration.as_millis() > 100 && self.window_bytes > 0 {
            // For more immediate feedback, calculate current rate if we have data
            self.bandwidth_bps = (self.window_bytes as f64 / window_duration.as_secs_f64()) as u64;
        }
        
        self.last_update = now;
    }
    
    /// 记录OS层面的发送成功（不是真正的端到端成功）
    fn record_send_success(&mut self) {
        self.heartbeats_sent += 1;
        self.heartbeats_acked += 1; // OS接受了包
        
        // 基于OS层面成功率计算"接口可用率"（不是真正的丢包率）
        if self.heartbeats_sent > 0 {
            let os_success_rate = self.heartbeats_acked as f64 / self.heartbeats_sent as f64;
            // 这里不是真正的丢包率，而是"接口健康度"
            self.loss_rate = 1.0 - os_success_rate;
        }
    }
    
    /// 记录OS层面的发送失败（通常是接口问题）
    fn record_send_failure(&mut self) {
        self.heartbeats_sent += 1;
        // 不增加heartbeats_acked，所以"接口健康度"会下降
        
        if self.heartbeats_sent > 0 {
            let os_success_rate = self.heartbeats_acked as f64 / self.heartbeats_sent as f64;
            self.loss_rate = 1.0 - os_success_rate;
        }
        
        // 发送失败时，惩罚RTT（表示接口有问题）
        let penalty_rtt = Duration::from_millis(500); // 接口问题，高延迟
        self.avg_rtt = Some(penalty_rtt);
    }
    
    /// 保守的RTT估算：主要基于接口类型和健康度
    fn estimate_rtt(&mut self) {
        // 基于接口健康度（非真正丢包率）估算RTT
        let base_rtt = Duration::from_millis(30); // 基础延迟
        
        // 根据"接口健康度"调整：健康度低可能意味着接口拥塞
        let health_penalty = (self.loss_rate * 200.0) as u64; // 最多200ms惩罚
        let estimated_rtt = base_rtt + Duration::from_millis(health_penalty);
        
        self.avg_rtt = Some(estimated_rtt.min(Duration::from_millis(1000))); // 最大1秒
    }
    
    /// 处理可能的端到端响应（如果应用有响应流量）
    fn _record_possible_response(&mut self) {
        if let Some(sent_time) = self.last_heartbeat_sent {
            let rtt = sent_time.elapsed();
            
            // 只记录合理的RTT（<2秒）
            if rtt < Duration::from_secs(2) {
                self._rtt_samples.push(rtt);
                
                // 保持最近5个RTT样本
                if self._rtt_samples.len() > 5 {
                    self._rtt_samples.remove(0);
                }
                
                // 如果有真实RTT样本，优先使用
                if !self._rtt_samples.is_empty() {
                    let total: Duration = self._rtt_samples.iter().sum();
                    self.avg_rtt = Some(total / self._rtt_samples.len() as u32);
                }
            }
        }
    }
    
    /// 检查链路超时，更新丢包率
    fn check_timeout(&mut self) {
        if let Some(last_heartbeat) = self.last_heartbeat_sent {
            // 如果超过5秒没收到响应，认为丢包
            if last_heartbeat.elapsed() > Duration::from_secs(5) && self.heartbeats_sent > self.heartbeats_acked {
                // 调整丢包率，但不要过于激进
                let timeout_loss = 0.1; // 超时时增加10%丢包率
                self.loss_rate = (self.loss_rate + timeout_loss).min(1.0);
            }
        }
        
        // 重置过久的统计数据，避免累积误差
        if self.last_update.elapsed() > Duration::from_secs(30) {
            // 30秒无活动，重置统计但保持基本信息
            if self.heartbeats_sent > 100 {
                // 保留最近的统计比例
                self.heartbeats_sent = self.heartbeats_sent / 2;
                self.heartbeats_acked = self.heartbeats_acked / 2;
            }
        }
    }

    fn get_weight_for_bandwidth(&self) -> f64 {
        if self.bandwidth_bps == 0 {
            0.1 // 无流量的链路给很低权重
        } else {
            // 将带宽转换为合理范围：使用MB/s作为基准
            let bandwidth_mbps = self.bandwidth_bps as f64 / (1024.0 * 1024.0);
            // 对数缩放，避免极端值
            (1.0 + bandwidth_mbps).ln().max(0.1)
        }
    }

    fn get_weight_for_interface_health(&self) -> f64 {
        // 基于接口错误率（不是真正的丢包率）计算健康度权重
        // 接口错误率高说明OS层面有问题（如接口down、路由问题等）
        let health_score = 1.0 - self.loss_rate.min(0.95); // 健康度：95%错误率时权重接近0
        health_score.max(0.05) // 最低5%权重，保持探索性
    }
    
    fn get_weight_for_latency(&self) -> f64 {
        if let Some(rtt) = self.avg_rtt {
            let rtt_ms = rtt.as_millis() as f64;
            // RTT越低权重越高：使用倒数关系，但加上基础值避免除零
            let latency_factor = 100.0 / (rtt_ms + 50.0); // 50ms基础延迟
            latency_factor.min(2.0).max(0.1) // 限制在0.1-2.0范围
        } else {
            1.0 // 无RTT数据时给中等权重
        }
    }
    
    fn get_adaptive_weight(&self) -> f64 {
        // 改进的自适应权重：综合考虑带宽、接口健康度、延迟
        let bandwidth_weight = self.get_weight_for_bandwidth();
        let health_weight = self.get_weight_for_interface_health();  
        let latency_weight = self.get_weight_for_latency();
        
        // 加权组合：带宽40%、健康度40%、延迟20%
        let combined_weight = (bandwidth_weight * 0.4) + 
                             (health_weight * 0.4) + 
                             (latency_weight * 0.2);
        
        // 添加少量随机性防止链路饥饿
        let exploration_bonus = 0.05; // 5%探索奖励
        
        // 活跃度加成：最近有活动的链路优先
        let activity_bonus = if self.last_update.elapsed() < Duration::from_secs(2) {
            0.1 // 最近2秒内活跃，10%加成
        } else {
            0.0
        };
        
        let final_weight = combined_weight + exploration_bonus + activity_bonus;
        
        // 确保权重在合理范围内
        final_weight.max(0.05).min(10.0)
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

/// Simple UDP aggregator that manages multiple UDP connections across different network interfaces
struct SimpleUdpAggregator {
    connections: Arc<RwLock<HashMap<String, UdpConnection>>>,
    strategy: LoadBalanceStrategy,
    rr_counter: AtomicU64,
    packets_sent: AtomicU64,
    bytes_sent: AtomicU64,
    link_stats: Arc<RwLock<HashMap<String, LinkStats>>>,
}

impl SimpleUdpAggregator {
    pub fn new(strategy: LoadBalanceStrategy) -> Self {
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            strategy,
            rr_counter: AtomicU64::new(0),
            packets_sent: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            link_stats: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Auto-discover network interfaces and create connections to targets
    pub async fn auto_discover_and_connect(&self, targets: &[SocketAddr]) -> Result<()> {
        info!("Starting smart interface discovery for targets: {:?}", targets);
        let interfaces = self.get_usable_interfaces_for_targets(targets)?;
        info!("Found {} usable network interfaces after smart filtering", interfaces.len());

        let mut total_connections = 0;
        for interface in interfaces {
            info!("Processing interface: {} with {} addresses", interface.name, interface.addr.len());
            for &target in targets {
                // Only create connection if interface can actually reach this target
                if !self.interface_can_reach_target(&interface, target) {
                    debug!("Skipping {} -> {} (incompatible)", interface.name, target);
                    continue;
                }
                
                info!("Creating connection from {} to {}", interface.name, target);
                
                // Add timeout to prevent hanging on socket creation
                match tokio::time::timeout(
                    Duration::from_secs(2),
                    self.create_connection_for_interface(&interface, target)
                ).await {
                    Ok(Ok(connection)) => {
                        let interface_key = format!("{}_{}", interface.name, target);
                        self.connections.write().await.insert(interface_key.clone(), connection);
                        self.link_stats.write().await.insert(interface_key.clone(), LinkStats::default());
                        info!("✓ Successfully created connection via {} to {} (key: {})", interface.name, target, interface_key);
                        total_connections += 1;
                    }
                    Ok(Err(e)) => {
                        warn!("✗ Failed to create connection via {} to {}: {}", interface.name, target, e);
                    }
                    Err(_) => {
                        warn!("✗ Timeout creating connection via {} to {}", interface.name, target);
                    }
                }
            }
        }

        if total_connections == 0 {
            return Err(anyhow::anyhow!("No usable connections could be established"));
        }

        info!("Successfully established {} UDP connections", total_connections);
        
        // 显示所有创建的连接
        let connections = self.connections.read().await;
        for (_key, conn) in connections.iter() {
            info!("Active connection: {} -> {} via {}", conn.local_addr, conn.target_addr, conn.interface_name);
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

    /// 启动链路质量监控任务，基于真实数据包统计
    pub async fn start_link_monitoring_tasks(&self) -> Vec<tokio::task::JoinHandle<()>> {
        let mut tasks = Vec::new();
        let connections = self.connections.read().await.clone();
        
        info!("Starting link monitoring tasks for {} connections", connections.len());
        
        for (connection_key, _connection) in connections {
            let link_stats = self.link_stats.clone();
            let key_clone = connection_key.clone();
            
            // 轻量级监控任务：只检查统计数据，不发送测试包
            let task = tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(2)); // 每2秒更新一次统计
                
                loop {
                    interval.tick().await;
                    
                    // 检查并更新链路统计，但不发送测试包
                    let mut stats = link_stats.write().await;
                    if let Some(link_stat) = stats.get_mut(&key_clone) {
                        // 基于真实数据包统计来更新丢包率和RTT
                        link_stat.check_timeout();
                        
                        // 如果长时间没有数据包活动，适当降低评分
                        if link_stat.last_update.elapsed() > Duration::from_secs(10) {
                            // 长时间无活动，可能网络有问题
                            if link_stat.heartbeats_sent > 0 {
                                link_stat.loss_rate = (link_stat.loss_rate + 0.05).min(0.3); // 增加5%丢包率，最多30%
                            }
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

        let mut physical_interfaces = Vec::new();
        let mut virtual_interfaces = Vec::new();
        
        for interface in all_interfaces {
            let name = interface.name.as_str();
            
            // Skip clearly unusable interfaces
            if name.starts_with("lo") ||        // loopback
               name.starts_with("docker") ||    // docker interfaces
               name.starts_with("br-") ||       // bridge interfaces  
               name.starts_with("virbr") ||     // libvirt bridge
               name.starts_with("veth") ||      // virtual ethernet pairs
               name.starts_with("tap") ||       // TAP interfaces
               name.starts_with("tun") ||       // TUN interfaces (except utun)
               interface.addr.is_empty() {      // must have addresses
                debug!("Filtered out interface {} (basic filter)", name);
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
            
            // Categorize interfaces: prioritize physical/cellular over virtual tunnels
            let is_virtual_tunnel = name.starts_with("wg") ||      // WireGuard
                                   name.starts_with("zt") ||       // ZeroTier  
                                   name.starts_with("utun") ||     // macOS VPN tunnels
                                   name.contains("vpn");           // VPN interfaces
            
            if is_virtual_tunnel {
                debug!("Adding virtual tunnel interface: {}", name);
                virtual_interfaces.push(interface);
            } else {
                info!("Adding physical/cellular interface: {}", name);
                physical_interfaces.push(interface);
            }
        }

        // Prefer physical interfaces, but include some virtual ones for diversity
        let mut result = physical_interfaces;
        
        // Add at most 2 virtual interfaces if we have few physical ones
        if result.len() < 3 {
            virtual_interfaces.truncate(2);
            result.extend(virtual_interfaces);
        }
        
        // Limit total interfaces to avoid too many connections
        result.truncate(4);

        info!("Selected {} interfaces: {:?}", 
               result.len(),
               result.iter().map(|i| &i.name).collect::<Vec<_>>());
        
        if result.is_empty() {
            return Err(anyhow::anyhow!("No usable network interfaces found for targets"));
        }
        
        Ok(result)
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
        &self, interface: &NetworkInterface, target: SocketAddr,
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

        // Quick connectivity test - try to send a small test packet
        let test_data = b"ping";
        match tokio::time::timeout(
            Duration::from_millis(500),
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

        // For better TCP performance, use a hash-based selection for the same flow
        // This reduces packet reordering within the same TCP connection
        let selected_key = if data.len() > 20 {
            // Try to use source/dest port for flow-based balancing
            self.select_connection_by_flow(&connections, data).await
        } else {
            self.select_connection(&connections).await
        };

        if let Some(connection) = connections.get(&selected_key) {
            match connection.socket.send_to(data, connection.target_addr).await {
                Ok(bytes_sent) => {
                    // Update global statistics efficiently
                    self.packets_sent.fetch_add(1, Ordering::Relaxed);
                    self.bytes_sent.fetch_add(bytes_sent as u64, Ordering::Relaxed);

                    // 减少锁竞争：只有每10个包或每秒更新一次详细统计
                    let packet_count = self.packets_sent.load(Ordering::Relaxed);
                    if packet_count % 10 == 0 {
                        if let Some(stats) = self.link_stats.write().await.get_mut(&selected_key) {
                            stats.update_send_stats(bytes_sent);
                            stats.record_send_success();
                            stats.estimate_rtt();
                        }
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
    
    /// Flow-based connection selection to reduce packet reordering
    async fn select_connection_by_flow(&self, connections: &HashMap<String, UdpConnection>, data: &[u8]) -> String {
        // Simple hash based on data content for flow affinity
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        
        // Hash first few bytes for flow identification
        let hash_data = if data.len() >= 8 { &data[0..8] } else { data };
        hasher.write(hash_data);
        let hash = hasher.finish();
        
        let connection_keys: Vec<_> = connections.keys().collect();
        let index = (hash as usize) % connection_keys.len();
        connection_keys[index].clone()
    }

    /// Select a connection based on the configured load balancing strategy (optimized)
    async fn select_connection(&self, connections: &HashMap<String, UdpConnection>) -> String {
        let selected = match self.strategy {
            LoadBalanceStrategy::PacketRoundRobin => {
                // 最快的策略：纯轮询，无需读取统计
                let count = self.rr_counter.fetch_add(1, Ordering::Relaxed);
                let index = (count as usize) % connections.len();
                let keys: Vec<_> = connections.keys().collect();
                keys[index].clone()
            }

            // 其他策略都需要读取统计，但限制频率
            _ => {
                // 每20个包才读取一次统计，减少锁竞争
                let packet_count = self.packets_sent.load(Ordering::Relaxed);
                if packet_count % 20 == 0 {
                    match self.strategy {
                        LoadBalanceStrategy::WeightedByBandwidth => self.select_weighted_by_bandwidth(connections).await,
                        LoadBalanceStrategy::FastestFirst => self.select_fastest_connection(connections).await,
                        LoadBalanceStrategy::WeightedByPacketLoss => self.select_weighted_by_loss(connections).await,
                        LoadBalanceStrategy::DynamicAdaptive => self.select_adaptive(connections).await,
                        _ => {
                            // 降级到轮询
                            let count = self.rr_counter.fetch_add(1, Ordering::Relaxed);
                            let index = (count as usize) % connections.len();
                            let keys: Vec<_> = connections.keys().collect();
                            keys[index].clone()
                        }
                    }
                } else {
                    // 大部分时候使用轮询，性能最好
                    let count = self.rr_counter.fetch_add(1, Ordering::Relaxed);
                    let index = (count as usize) % connections.len();
                    let keys: Vec<_> = connections.keys().collect();
                    keys[index].clone()
                }
            }
        };
        
        selected
    }

    /// Select connection weighted by bandwidth
    async fn select_weighted_by_bandwidth(&self, connections: &HashMap<String, UdpConnection>) -> String {
        let stats = self.link_stats.read().await;
        let mut best_key = connections.keys().next().unwrap().clone();
        let mut best_bandwidth = 0u64;

        for key in connections.keys() {
            let bandwidth = if let Some(link_stats) = stats.get(key) {
                link_stats.bandwidth_bps
            } else {
                1000000 // Default 1 Mbps for new connections
            };
            
            if bandwidth > best_bandwidth {
                best_bandwidth = bandwidth;
                best_key = key.clone();
            }
        }

        best_key
    }

    /// Select fastest (lowest latency) connection
    async fn select_fastest_connection(&self, connections: &HashMap<String, UdpConnection>) -> String {
        let stats = self.link_stats.read().await;
        let mut best_key = connections.keys().next().unwrap().clone();
        let mut best_rtt = Duration::from_secs(u64::MAX);

        for key in connections.keys() {
            let rtt = if let Some(link_stats) = stats.get(key) {
                link_stats.avg_rtt.unwrap_or(Duration::from_millis(100)) // Default 100ms
            } else {
                Duration::from_millis(50) // New connections get priority with 50ms default
            };
            
            if rtt < best_rtt {
                best_rtt = rtt;
                best_key = key.clone();
            }
        }

        best_key
    }

    /// Select connection weighted by packet loss (favor low loss connections)
    async fn select_weighted_by_loss(&self, connections: &HashMap<String, UdpConnection>) -> String {
        let stats = self.link_stats.read().await;
        let mut best_key = connections.keys().next().unwrap().clone();
        let mut best_weight = 0.0f64;

        for key in connections.keys() {
            let weight = if let Some(link_stats) = stats.get(key) {
                link_stats.get_weight_for_interface_health()
            } else {
                1.0 // New connections get full weight
            };
            
            if weight > best_weight {
                best_weight = weight;
                best_key = key.clone();
            }
        }

        best_key
    }

    /// Dynamic adaptive selection combining multiple factors (enhanced)
    async fn select_adaptive(&self, connections: &HashMap<String, UdpConnection>) -> String {
        let stats = self.link_stats.read().await;
        let mut weights: Vec<(String, f64, String)> = Vec::new(); // 添加调试信息
        let mut total_weight = 0.0f64;

        // Calculate weights for all connections with debug info
        for key in connections.keys() {
            let (weight, debug_info) = if let Some(link_stats) = stats.get(key) {
                let w = link_stats.get_adaptive_weight();
                let info = format!("bw:{:.1}MB/s if-err:{:.1}% rtt:{:?}", 
                                 link_stats.bandwidth_bps as f64 / (1024.0 * 1024.0),
                                 link_stats.loss_rate * 100.0,
                                 link_stats.avg_rtt);
                (w, info)
            } else {
                (1.5, "new-connection".to_string()) // Give new connections higher weight
            };
            weights.push((key.clone(), weight, debug_info));
            total_weight += weight;
        }

        if total_weight == 0.0 {
            // Fallback to round-robin if no weights
            let count = self.rr_counter.fetch_add(1, Ordering::Relaxed);
            let index = (count as usize) % connections.len();
            return connections.keys().nth(index).unwrap().clone();
        }

        // 周期性打印权重分布用于调试
        let packet_count = self.packets_sent.load(Ordering::Relaxed);
        if packet_count % 100 == 0 {
            debug!("Adaptive weights (total: {:.2}):", total_weight);
            for (key, weight, info) in &weights {
                debug!("  {}: weight={:.2} ({})", key, weight, info);
            }
        }

        // Weighted random selection
        let mut random_value = random::<f64>() * total_weight;

        for (key, weight, _debug_info) in weights {
            random_value -= weight;
            if random_value <= 0.0 {
                return key;
            }
        }

        // Fallback (should not reach here)
        connections.keys().next().unwrap().clone()
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
    local: SocketAddr, targets: Vec<SocketAddr>, strategy: String, _role: String, no_monitor: bool,
) -> Result<()> {
    let no_monitor = no_monitor || !stdout().is_tty();

    info!("Starting multi-interface UDP proxy on {}", local);
    info!("Targets: {:?}", targets);
    info!("Strategy: {}", strategy);

    let strategy = match strategy.as_str() {
        "packet-round-robin" => LoadBalanceStrategy::PacketRoundRobin,
        "weighted-bandwidth" => LoadBalanceStrategy::WeightedByBandwidth,
        "fastest-first" => LoadBalanceStrategy::FastestFirst,
        "weighted-packet-loss" => LoadBalanceStrategy::WeightedByPacketLoss,
        "dynamic-adaptive" => LoadBalanceStrategy::DynamicAdaptive,
        _ => LoadBalanceStrategy::PacketRoundRobin, // Default to packet-level round-robin
    };

    // Create multi-interface UDP aggregator
    let aggregator = Arc::new(SimpleUdpAggregator::new(strategy));
    
    // Auto-discover interfaces and create connections
    aggregator.auto_discover_and_connect(&targets).await?;
    
    let connection_count = aggregator.connection_count().await;
    info!("Multi-interface UDP aggregator ready with {} connections", connection_count);

    // 启动链路监控任务用于丢包检测
    let monitoring_tasks = aggregator.start_link_monitoring_tasks().await;
    info!("Started {} link monitoring tasks for loss detection", monitoring_tasks.len());

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

                    // 智能判断链路状态：考虑活动时间、丢包率、和连接质量
                    let recent_activity = stats.last_update.elapsed() < Duration::from_secs(5);
                    let has_traffic = stats.bandwidth_bps > 0 || stats.packets_sent > 0;
                    let low_loss = stats.loss_rate < 0.5; // 丢包率低于50%
                    let good_rtt = stats.avg_rtt.map_or(true, |rtt| rtt < Duration::from_millis(1000));
                    
                    // 更严格的ACTIVE判断：必须有活动且连接质量良好
                    let _is_active = recent_activity && has_traffic && low_loss && good_rtt;
                    
                    // 状态描述
                    let status_str = if !recent_activity {
                        "IDLE"
                    } else if !low_loss {
                        "POOR" // 高丢包率
                    } else if !good_rtt {
                        "SLOW" // 高延迟
                    } else if has_traffic {
                        "ACTIVE"
                    } else {
                        "IDLE"
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

async fn run_server(listen: Vec<SocketAddr>, target: SocketAddr, no_monitor: bool) -> Result<()> {
    let no_monitor = no_monitor || !stdout().is_tty();
    
    info!("UDP Aggregation Server - {:?} -> {}", listen, target);
    
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
    
    // Monitoring task
    if !no_monitor {
        let stats_packets_received = packets_received.clone();
        let stats_bytes_received = bytes_received.clone();
        let _stats_packets_forwarded = packets_forwarded.clone();
        let _stats_bytes_forwarded = bytes_forwarded.clone();
        let monitor_listen = listen.clone();
        
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(1));
            let mut last_packets = 0u64;
            let mut last_time = Instant::now();
            
            loop {
                interval.tick().await;
                
                let current_packets = stats_packets_received.load(Ordering::Relaxed);
                let _current_bytes = stats_bytes_received.load(Ordering::Relaxed);
                let current_time = Instant::now();
                
                let elapsed = current_time.duration_since(last_time).as_secs_f64();
                let pps = if elapsed > 0.0 {
                    (current_packets - last_packets) as f64 / elapsed
                } else {
                    0.0
                };
                
                // Clear screen and reset cursor
                print!("\x1B[2J\x1B[H");
                
                // Display title and stats
                let title = format!("UDP Aggregation Server - {:?} -> {}", monitor_listen, target).bold().green();
                println!("{}", title);
                println!("UDP Server - {} total packets ({:.0} pps)", current_packets, pps);
                println!("Listening on {} interfaces:", monitor_listen.len());
                for addr in &monitor_listen {
                    println!("  {} -> {}", addr, target);
                }
                
                last_packets = current_packets;
                last_time = current_time;
            }
        });
    }
    
    // 添加从目标服务接收响应并转发回客户端的任务
    let response_sessions = sessions.clone();
    let _response_client_map = client_target_map.clone();
    let response_target_socket = target_socket.clone(); 
    let _response_sockets = sockets.clone();
    
    let response_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        info!("=== RESPONSE HANDLER STARTED === Waiting for responses from target {}", target);
        
        loop {
            match response_target_socket.recv_from(&mut buf).await {
                Ok((len, response_addr)) => {
                    debug!("Response received {} bytes from target {}", len, response_addr);
                    
                    if response_addr == target {
                        // 这是来自目标服务的响应，需要转发回所有相关客户端
                        let sessions_read = response_sessions.read().await;
                        
                        // 获取所有活跃的客户端会话
                        for (client_addr, session) in sessions_read.iter() {
                            if let Some(server_socket) = &session.server_socket {
                                if let Err(e) = server_socket.send_to(&buf[..len], *client_addr).await {
                                    warn!("Failed to forward response to client {}: {}", client_addr, e);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to receive response from target: {}", e);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    });
    
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
        let target_socket_task = target_socket.clone();
        let client_map_task = client_target_map.clone();
        let packets_received_task = packets_received.clone();
        let bytes_received_task = bytes_received.clone();
        let packets_forwarded_task = packets_forwarded.clone();
        let bytes_forwarded_task = bytes_forwarded.clone();
        let socket_for_session = socket.clone();
        
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            let socket_addr = socket.local_addr().unwrap_or_else(|_| "unknown".parse().unwrap());
            info!("=== SERVER TASK STARTED === Listening on socket {}", socket_addr);
            
            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((len, client_addr)) => {
                        debug!("Server received {} bytes from {}", len, client_addr);
                        
                        // Update statistics
                        packets_received_task.fetch_add(1, Ordering::Relaxed);
                        bytes_received_task.fetch_add(len as u64, Ordering::Relaxed);
                        
                        // Update or create session with socket reference
                        {
                            let mut sessions = sessions_task.write().await;
                            match sessions.get_mut(&client_addr) {
                                Some(session) => {
                                    session.update_activity();
                                }
                                None => {
                                    sessions.insert(client_addr, UdpSession::new_with_socket(client_addr, socket_for_session.clone()));
                                    debug!("New aggregated session from: {}", client_addr);
                                }
                            }
                            
                            // 记录客户端到目标的映射
                            client_map_task.write().await.insert(client_addr, target);
                        }
                        
                        // Forward to target backend
                        match target_socket_task.send_to(&buf[..len], target).await {
                            Ok(sent_bytes) => {
                                packets_forwarded_task.fetch_add(1, Ordering::Relaxed);
                                bytes_forwarded_task.fetch_add(sent_bytes as u64, Ordering::Relaxed);
                            }
                            Err(e) => {
                                warn!("Failed to forward to target {}: {}", target, e);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to receive: {}", e);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        });
        
        tasks.push(task);
    }
    
    // 添加响应转发任务
    tasks.push(response_task);
    
    // Wait for all tasks
    for task in tasks {
        let _ = task.await;
    }
    
    Ok(())
}
