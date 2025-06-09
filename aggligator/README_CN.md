# Aggligator — 你的友好链路聚合器

[![crates.io page](https://img.shields.io/crates/v/aggligator)](https://crates.io/crates/aggligator)
[![docs.rs page](https://docs.rs/aggligator/badge.svg)](https://docs.rs/aggligator)
[![Apache 2.0 license](https://img.shields.io/crates/l/aggligator)](https://raw.githubusercontent.com/surban/aggligator/master/LICENSE)

Aggligator 是一个用纯Rust编写的链路聚合库，它将多个网络链路聚合成一个连接，提供带宽叠加和链路冗余功能。

## UDP聚合 vs TCP聚合的设计理念

### TCP聚合（当前实现）

- **严格顺序保证**：所有数据包必须按序传递给应用层
- **可靠性优先**：重传、确认、顺序重组
- **延迟权衡**：为了可靠性可能增加延迟
- **适用场景**：文件传输、HTTP、数据库连接等

### UDP聚合（新设计方向）

- **无顺序约束**：数据包立即传递，不等待重排序
- **性能优先**：最小延迟，最大吞吐
- **应用层负责**：数据完整性和顺序由应用层处理
- **适用场景**：实时音视频、游戏、IoT数据流等

```rust
// UDP聚合的核心理念
trait UdpAggregator {
    // 立即发送，不等待确认
    fn send_immediate(&mut self, data: Bytes) -> Result<(), SendError>;

    // 立即接收，不保证顺序
    fn recv_immediate(&mut self) -> Result<(LinkId, Bytes), RecvError>;

    // 简单的链路状态监控
    fn link_stats(&self) -> Vec<LinkStats>;
}
```

## 实现进度

### ✅ 第一步：无序聚合配置系统 (`unordered_cfg.rs`)

已完成基础的无序聚合配置系统，包括：

- **负载均衡策略**：
  - **数据包级负载均衡（真正的多链路并行传输）：**
    - 分包轮询（PacketRoundRobin）：数据包级别轮询，每包重新选择链路，实现真正的带宽聚合
    - 丢包权重（WeightedByPacketLoss）：基于丢包率的加权随机选择，每包动态调整链路使用概率
    - 自适应质量（AdaptiveQuality）：综合带宽和丢包率的质量评分选择，每包重新评估最优链路
    - 动态自适应（DynamicAdaptive）：**最先进的策略**，滑动窗口统计+加权随机选择+探索机制，每包都重新计算实时权重并智能选择，实现真正的多链路并行传输和自动故障恢复
    - 轮询（RoundRobin）：按轮询顺序为每个数据包选择链路
    - 带宽权重（WeightedByBandwidth）：按链路带宽为每个数据包选择最佳链路
    - 最快优先（FastestFirst）：为每个数据包选择最低延迟链路
    - 随机分布（Random）：为每个数据包随机分配链路

**重大改进**：🚀 **已移除所有会话亲和性限制！**

- 所有负载均衡策略现在都在数据包级别工作
- 每个数据包都可以根据策略算法选择最优链路
- 实现真正的多链路带宽聚合和智能故障转移
- `DynamicAdaptive` 策略能根据实时链路质量动态调整权重，提供最佳性能

- **简化配置**：移除了有序聚合中复杂的顺序保证相关配置
- **预设配置**：低延迟、高吞吐、不可靠网络等场景的优化配置
- **配置验证**：确保配置参数的合理性

### ✅ 第二步：无序聚合消息系统 (`unordered_msg.rs`)

已完成无序聚合的消息协议定义，包括：

- **简化消息类型**：移除了有序聚合中的顺序号、确认机制
- **轻量级协议**：最小化消息开销，优化性能
- **链路管理**：连接建立、心跳检测、状态同步
- **序列化优化**：高效的二进制序列化/反序列化

### ✅ 第三步：无序聚合任务系统 (`unordered_task.rs`) - **协议无关设计**

**重要设计决策**：我们采用了协议无关的抽象设计，而不是在核心库中绑定具体的UDP socket实现。

#### 传输抽象层设计

```rust
/// 协议无关的传输接口
#[async_trait::async_trait]
pub trait UnorderedLinkTransport: Send + Sync + Debug + 'static {
    /// 发送数据
    async fn send(&self, data: &[u8]) -> Result<usize, std::io::Error>;

    /// 获取远端地址（用于标识和日志）
    fn remote_addr(&self) -> String;

    /// 检查链路健康状态
    async fn is_healthy(&self) -> bool;
}
```

#### 核心组件

- **UnorderedLinkState**：链路状态管理（协议无关）
- **UnorderedAggTask**：聚合任务主逻辑
- **UnorderedLoadBalancer**：负载均衡算法实现
- **UnorderedAggHandle**：外部API接口

#### 设计优势

1. **协议无关**：可以支持UDP、QUIC、WebRTC等无序传输协议
2. **传输分离**：具体的socket实现由transport crates提供（如`aggligator-transport-udp`）
3. **易于测试**：通过Mock transport轻松进行单元测试
4. **架构一致**：延续Aggligator现有的传输抽象设计理念

### ✅ 命名重构：从UDP特定到协议无关

**重要里程碑**：我们完成了从UDP特定命名到协议无关命名的重构：

#### 文件重命名

- `udp_cfg.rs` → `unordered_cfg.rs`
- `udp_msg.rs` → `unordered_msg.rs`
- `udp_task.rs` → `unordered_task.rs`

#### 类型重命名

- `UdpCfg` → `UnorderedCfg`
- `UdpLinkMsg` → `UnorderedLinkMsg`
- `UdpLinkTransport` → `UnorderedLinkTransport`
- `UdpAggTask` → `UnorderedAggTask`
- `UdpAggHandle` → `UnorderedAggHandle`
- 以及所有相关的类型和trait

#### 重构意义

这次重构体现了设计理念的成熟：

- **从协议绑定到协议抽象**：不再局限于UDP协议
- **更准确的语义**：`unordered` 更好地描述了核心特性（无序聚合）
- **更广泛的适用性**：可以支持所有无序传输协议
- **架构一致性**：与Aggligator现有的transport抽象保持一致

```rust
// 使用示例
let cfg = UdpCfg::low_latency();  // 低延迟优化
let cfg = UdpCfg::high_throughput();  // 高吞吐优化
let cfg = UdpCfg::unreliable_network();  // 不稳定网络优化
```

### 🔄 下一步：具体传输实现

现在无序聚合的核心抽象已经完成，下一步将：

1. **创建 UDP 传输实现**：在独立的 crate 中实现 `UnorderedLinkTransport` trait for UDP
2. **实现示例和文档**：提供完整的使用示例和最佳实践
3. **性能优化和测试**：进行大规模测试和性能调优
4. **扩展其他协议**：支持 QUIC、WebRTC 等其他无序传输协议

## 概述

Aggligator 接收两个端点之间的多个网络链路（例如TCP连接），并将它们合并成一个具有所有链路综合带宽的连接。它还提供对单个链路故障的恢复能力，并允许动态添加和删除链路。

它与[Multipath TCP]和[SCTP]有相同的目的，但可以在现有的、广泛采用的协议（如TCP、HTTPS、TLS、USB和WebSockets）上工作，完全在用户空间中实现，无需操作系统的任何支持。

核心特性：

- **100% 安全的Rust代码**：使用Rust的内存安全特性避免常见的网络编程错误
- **基于Tokio异步运行时**：高性能异步IO处理
- **跨平台支持**：支持所有主流本地平台和WebAssembly
- **协议无关**：可以在任何可靠的传输层上工作
- **动态链路管理**：支持运行时添加和删除链路
- **自动故障恢复**：单个链路失败时自动切换到其他链路

[Multipath TCP]: https://en.wikipedia.org/wiki/Multipath_TCP
[SCTP]: https://en.wikipedia.org/wiki/Stream_Control_Transmission_Protocol

## 核心架构

### 1. 配置系统 (`cfg.rs`)

配置系统控制连接的各种参数：

```rust
/// 链路聚合连接配置
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cfg {
    /// 使用流式IO时数据包的大小
    pub io_write_size: NonZeroUsize,
    /// 发送缓冲区大小 - 未确认发送字节的最大数量
    pub send_buffer: NonZeroU32,
    /// 发送队列长度
    pub send_queue: NonZeroUsize,
    /// 接收缓冲区大小 - 未确认接收字节的最大数量
    pub recv_buffer: NonZeroU32,
    /// 接收队列长度
    pub recv_queue: NonZeroUsize,
    /// 等待数据包确认的最小超时时间
    pub link_ack_timeout_min: Duration,
    /// 从往返时间计算确认超时的因子
    pub link_ack_timeout_roundtrip_factor: NonZeroU32,
    /// 等待数据包确认的最大超时时间
    pub link_ack_timeout_max: Duration,
    /// 每个链路未确认发送数据的最大量
    pub link_unacked_limit: NonZeroUsize,
    /// 链路ping模式
    pub link_ping: LinkPing,
    // ... 更多配置参数
}
```

默认配置已经针对高达100 MB/s的连接进行了优化：

```rust
impl Default for Cfg {
    fn default() -> Self {
        Self {
            io_write_size: NonZeroUsize::new(8_192).unwrap(),
            send_buffer: NonZeroU32::new(67_108_864).unwrap(), // 64MB
            recv_buffer: NonZeroU32::new(67_108_864).unwrap(), // 64MB
            link_unacked_limit: NonZeroUsize::new(33_554_432).unwrap(), // 32MB
            link_ping: LinkPing::WhenIdle(Duration::from_secs(15)),
            // ...
        }
    }
}
```

### 2. 连接建立 (`connect.rs`)

连接建立模块提供了服务器端和客户端的连接管理：

```rust
/// 链路聚合服务器
pub struct Server {
    // 内部实现隐藏
}

impl Server {
    pub fn new(cfg: Cfg) -> Self {
        // 创建新的服务器实例
    }

    /// 开始监听传入连接
    pub fn listen(&self) -> Result<Listener, ListenError> {
        // 返回监听器用于接受连接
    }

    /// 添加传入链路
    pub async fn add_incoming<TX, RX, TAG>(
        &self,
        tx: TX,
        rx: RX,
        tag: TAG,
        user_data: &[u8],
    ) -> Result<Link<TAG>, IncomingError>
    where
        TX: Sink<Bytes, Error = io::Error> + Unpin + Send + 'static,
        RX: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + 'static,
        TAG: Send + Sync + 'static,
    {
        // 链路握手和注册逻辑
    }
}
```

### 3. 聚合链路通道 (`alc/channel.rs`)

聚合链路通道是面向用户的API，提供消息传递和流式IO两种接口：

```rust
/// 基于聚合链路连接的双向通道
#[derive(Debug)]
pub struct Channel {
    cfg: Arc<Cfg>,
    remote_cfg: Option<Arc<ExchangedCfg>>,
    conn_id: ConnId,
    tx: mpsc::Sender<SendReq>,
    tx_error: watch::Receiver<SendError>,
    rx: mpsc::Receiver<Bytes>,
    rx_closed: mpsc::Sender<()>,
    rx_error: watch::Receiver<Option<RecvError>>,
}

impl Channel {
    /// 分割为消息发送器和接收器
    pub fn into_tx_rx(self) -> (Sender, Receiver) {
        // 创建消息接口
    }

    /// 转换为实现AsyncRead和AsyncWrite的流
    pub fn into_stream(self) -> Stream {
        let (tx, rx) = self.into_tx_rx();
        Stream {
            tx: tx.into_sink(),
            rx: rx.into_stream()
        }
    }
}

/// 实现AsyncRead和AsyncWrite的双向IO流
pub struct Stream {
    tx: SenderSink,
    rx: ReceiverStream,
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context, buf: &mut ReadBuf) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().rx).poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.get_mut().tx).poll_write(cx, buf)
    }
    // ... 其他AsyncWrite方法
}
```

### 4. 链路聚合任务 (`agg/task.rs`)

这是Aggligator的核心组件，负责实际的链路聚合逻辑：

```rust
pub struct Task<TX, RX, TAG> {
    // 1960行复杂的状态机实现
    // 处理多链路数据分发、确认、重传、故障检测等
}

/// 发送请求类型
pub(crate) enum SendReq {
    /// 发送数据
    Send(Bytes),
    /// 刷新
    Flush(oneshot::Sender<()>),
}

/// 任务错误类型
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskError {
    /// 所有链路长时间未确认
    AllUnconfirmedTimeout,
    /// 长时间没有可用链路
    NoLinksTimeout,
    /// 链路上发生协议错误
    ProtocolError {
        link_id: LinkId,
        error: String,
    },
    /// 链路连接到了不同的服务器
    ServerIdMismatch,
    /// 任务被终止
    Terminated,
}
```

任务的核心运行循环处理以下事件：

- 数据发送请求
- 链路状态变化
- 确认超时
- 链路健康检查
- 数据重组和重传

### 5. 控制接口 (`control.rs`)

控制接口提供运行时管理功能：

```rust
/// 连接控制接口
pub struct Control<TX, RX, TAG> {
    cfg: Arc<Cfg>,
    conn_id: ConnId,
    server_id: Option<ServerId>,
    direction: Direction,
    // ... 内部状态
}

impl<TX, RX, TAG> Control<TX, RX, TAG>
where
    TX: Sink<Bytes, Error = io::Error> + Unpin + Send + 'static,
    RX: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + 'static,
    TAG: Send + Sync + 'static,
{
    /// 添加新链路到连接
    pub async fn add_link(
        &self,
        tx: TX,
        rx: RX,
        tag: TAG,
    ) -> Result<Link<TAG>, AddLinkError> {
        // 链路握手和添加逻辑
    }

    /// 获取所有活跃链路
    pub fn links(&self) -> Vec<Link<TAG>> {
        // 返回当前链路列表
    }

    /// 检查连接是否已终止
    pub fn is_terminated(&self) -> bool {
        // 检查连接状态
    }

    /// 等待连接终止
    pub async fn wait_terminated(&self) -> TaskError {
        // 异步等待连接结束
    }
}
```

### 6. 传输层抽象 (`transport/`)

传输层模块提供了连接器和接受器的抽象，用于自动化多链路管理：

```rust
/// 连接器 - 用于建立出站连接
pub struct Connector {
    // 内部实现
}

impl Connector {
    /// 创建新的连接器
    pub fn new() -> Self {
        // 创建默认连接器
    }

    /// 添加传输层实现
    pub fn add(&self, transport: impl ConnectingTransport) -> ConnectingTransportHandle {
        // 添加传输层并返回句柄
    }

    /// 获取可用的链路标签
    pub fn available_tags(&self) -> HashSet<LinkTagBox> {
        // 返回当前可用的链路标签
    }

    /// 获取连接控制接口
    pub fn control(&self) -> BoxControl {
        // 返回控制接口
    }
}

/// 接受器 - 用于接受入站连接
pub struct Acceptor {
    // 内部实现
}

impl Acceptor {
    /// 创建新的接受器
    pub fn new() -> Self {
        // 创建默认接受器
    }

    /// 添加传输层实现
    pub fn add(&self, transport: impl AcceptingTransport) -> AcceptingTransportHandle {
        // 添加传输层并返回句柄
    }
}

/// 传输层接口 - 用于连接到远程端点
#[async_trait]
pub trait ConnectingTransport: Send + Sync + 'static {
    /// 传输层名称
    fn name(&self) -> &str;

    /// 发现可连接的链路标签
    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()>;

    /// 连接到指定的链路标签
    async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox>;
}

/// 传输层接口 - 用于接受来自远程端点的连接
#[async_trait]
pub trait AcceptingTransport: Send + Sync + 'static {
    /// 传输层名称
    fn name(&self) -> &str;

    /// 监听并接受传入连接
    async fn listen(&self, tx: mpsc::Sender<AcceptedStreamBox>) -> Result<()>;
}
```

### 7. IO 包装器 (`io/`)

IO 模块提供了将流式IO转换为数据包式IO的包装器：

```rust
/// 数据完整性编解码器
pub struct IntegrityCodec {
    // 内部实现
}

impl IntegrityCodec {
    /// 创建新的编解码器
    pub fn new() -> Self {
        // 使用默认配置
    }

    /// 设置最大数据包大小
    pub fn set_max_packet_size(&mut self, max_packet_size: u32) {
        // 设置数据包大小限制
    }
}

/// 发送包装器 - 将 AsyncWrite 转换为 Sink<Bytes>
pub struct IoTx<W>(pub FramedWrite<W, IntegrityCodec>);

impl<W: AsyncWrite> IoTx<W> {
    /// 使用默认编解码器包装写入器
    pub fn new(write: W) -> Self {
        // 创建带完整性检查的发送器
    }
}

/// 接收包装器 - 将 AsyncRead 转换为 Stream<Bytes>
pub struct IoRx<R>(pub FramedRead<R, IntegrityCodec>);

impl<R: AsyncRead> IoRx<R> {
    /// 使用默认编解码器包装读取器
    pub fn new(read: R) -> Self {
        // 创建带完整性检查的接收器
    }
}

/// 流式IO类型的统一包装器
pub enum StreamBox {
    TxRx(TxRxBox),
    Io(IoBox),
}
```

### 8. 便利函数

```rust
/// 创建出站连接的便利函数
pub fn connect<TX, RX, TAG>(cfg: Cfg) -> (Task<TX, RX, TAG>, Outgoing, Control<TX, RX, TAG>)
where
    TX: Sink<Bytes, Error = io::Error> + Unpin + Send + 'static,
    RX: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + 'static,
    TAG: Send + Sync + 'static,
{
    // 创建服务器并建立出站连接
}
```

## 使用示例

### 基本的客户端-服务器连接

```rust
use aggligator::{connect, Server, Cfg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 服务器端
    let server_task = tokio::spawn(async {
        let server = Server::new(Cfg::default());
        let mut listener = server.listen().unwrap();

        // 添加TCP链路 (需要传输层实现)
        // server.add_incoming(tcp_tx, tcp_rx, "tcp-link", &[]).await?;

        // 接受连接
        let incoming = listener.next().await.unwrap();
        let (task, channel, _control) = incoming.accept();
        tokio::spawn(task);

        // 使用连接
        let mut stream = channel.into_stream();
        let mut buffer = [0u8; 1024];
        let n = stream.read(&mut buffer).await?;
        println!("收到: {}", String::from_utf8_lossy(&buffer[..n]));

        Ok::<_, Box<dyn std::error::Error>>(())
    });

    // 客户端
    let client_task = tokio::spawn(async {
        let (channel, control) = connect(Cfg::default()).await?;

        // 添加链路到连接
        // control.add_link(tcp_tx, tcp_rx, "tcp-link").await?;

        // 使用连接
        let mut stream = channel.into_stream();
        stream.write_all(b"Hello, Aggligator!").await?;

        Ok::<_, Box<dyn std::error::Error>>(())
    });

    let (server_result, client_result) = tokio::join!(server_task, client_task);
    server_result??;
    client_result??;

    Ok(())
}
```

### 使用消息接口

```rust
use aggligator::{connect, Server, Cfg};
use bytes::Bytes;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (channel, _control) = connect(Cfg::default()).await?;
    let (sender, mut receiver) = channel.into_tx_rx();

    // 发送消息
    sender.send(Bytes::from("Hello")).await?;
    sender.send(Bytes::from("World")).await?;

    // 接收消息
    while let Some(msg) = receiver.recv().await? {
        println!("收到消息: {}", String::from_utf8_lossy(&msg));
    }

    Ok(())
}
```

### 链路故障处理

```rust
use aggligator::{Control, Link};

async fn monitor_links<TX, RX, TAG>(control: &Control<TX, RX, TAG>)
where
    TX: Sink<Bytes, Error = io::Error> + Unpin + Send + 'static,
    RX: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + 'static,
    TAG: Send + Sync + 'static,
{
    loop {
        let links = control.links();
        println!("当前活跃链路数: {}", links.len());

        for link in &links {
            if let Some(reason) = link.disconnect_reason() {
                println!("链路 {:?} 断开，原因: {:?}", link.tag(), reason);
            } else {
                let stats = link.stats();
                println!("链路 {:?} 状态: ping={:?}, 速度={} B/s",
                    link.tag(), stats.ping, stats.speed);
            }
        }

        if control.is_terminated() {
            println!("连接已终止");
            break;
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
```

## 性能优化

### 缓冲区配置

对于高带宽连接，调整缓冲区大小：

```rust
use std::num::{NonZeroU32, NonZeroUsize};

let mut cfg = Cfg::default();
// 增加缓冲区大小用于高带宽连接
cfg.send_buffer = NonZeroU32::new(134_217_728).unwrap(); // 128MB
cfg.recv_buffer = NonZeroU32::new(134_217_728).unwrap(); // 128MB
cfg.link_unacked_limit = NonZeroUsize::new(67_108_864).unwrap(); // 64MB per link
```

### 链路ping配置

```rust
use aggligator::cfg::LinkPing;

let mut cfg = Cfg::default();
// 配置链路ping行为
cfg.link_ping = LinkPing::Periodic(Duration::from_secs(10)); // 定期ping
// 或者
cfg.link_ping = LinkPing::WhenIdle(Duration::from_secs(30)); // 空闲时ping
// 或者
cfg.link_ping = LinkPing::WhenTimedOut; // 超时时ping
```

## 安全考虑

Aggligator本身不提供加密或身份验证。对于敏感数据，应该在传输层使用TLS等加密手段（需用户自行集成）。

此外，Aggligator提供连接标识符的加密：

- 使用Diffie-Hellman密钥交换生成共享密钥
- 连接标识符使用该密钥加密
- 防止攻击者通过伪造连接标识符注入恶意链路

## 错误处理

Aggligator定义了详细的错误类型：

```rust
use aggligator::{TaskError, control::AddLinkError};

// 连接级错误
match task_error {
    TaskError::AllUnconfirmedTimeout => {
        println!("所有链路长时间未确认，可能网络质量差");
    }
    TaskError::NoLinksTimeout => {
        println!("长时间没有可用链路，连接断开");
    }
    TaskError::ProtocolError { link_id, error } => {
        println!("链路 {} 协议错误: {}", link_id, error);
    }
    TaskError::ServerIdMismatch => {
        println!("服务器ID不匹配，可能服务器重启了");
    }
    TaskError::Terminated => {
        println!("连接正常终止");
    }
}

// 添加链路错误
match add_link_error {
    AddLinkError::ServerIdMismatch { expected, present } => {
        println!("服务器ID不匹配: 期望 {}, 实际 {}", expected, present);
    }
    AddLinkError::ConnectionRefused => {
        println!("连接被拒绝");
    }
    AddLinkError::Io(io_error) => {
        println!("IO错误: {}", io_error);
    }
    // ... 其他错误类型
}
```

## 调试和监控

### dump 功能

启用`dump`特性可以保存分析数据用于性能调试：

```rust
#[cfg(feature = "dump")]
use aggligator::dump::{ConnDump, LinkDump};

// 在 Task 上启用 dump
#[cfg(feature = "dump")]
task.dump(dump_tx);
```

dump 数据包含连接和链路的详细统计信息：

```rust
/// 连接dump数据
pub struct ConnDump {
    /// 连接ID
    pub conn_id: u128,
    /// 运行时间（秒）
    pub runtime: f32,
    /// 发送未确认数据量
    pub txed_unacked: usize,
    /// 发送缓冲区大小
    pub send_buffer: u32,
    /// 重发队列长度
    pub resend_queue: usize,
    /// 链路统计信息
    pub link0: LinkDump,
    pub link1: LinkDump,
    // ...
}

/// 链路dump数据
pub struct LinkDump {
    /// 链路是否存在
    pub present: bool,
    /// 链路ID
    pub link_id: u128,
    /// 是否未确认
    pub unconfirmed: bool,
    /// 发送器状态
    pub tx_idle: bool,
    /// 总发送字节数
    pub total_sent: u64,
    /// 总接收字节数
    pub total_recved: u64,
    /// 往返时间（毫秒）
    pub roundtrip: f32,
}
```

dump 数据可以使用仓库中的 `PlotDump.ipynb` 脚本进行可视化分析。

### ✅ 第四步：文档和注释协议无关化清理

**已完成**：全面完成了从UDP特定到协议无关的文档和注释清理：

#### 注释和文档更新

- **配置模块**：所有注释从"UDP聚合"更新为"无序聚合"
- **消息模块**：协议标识符和注释全部通用化
- **任务模块**：所有类型、函数、注释从UDP特定转为协议无关
- **测试用例**：所有单元测试的命名和内容协议无关化

#### 细节变更

- "UDP链路状态" → "无序链路状态"
- "UDP聚合任务" → "无序聚合任务"
- "UDP传输接口" → "无序传输接口"
- "UDP聚合统计" → "无序聚合统计"
- "UDP负载均衡" → "无序负载均衡"

#### 验证完成

- ✅ 所有代码编译通过
- ✅ 所有单元测试通过
- ✅ 导出接口完全更新
- ✅ 无遗留UDP特定引用

**设计意义**：此阶段确保了整个无序聚合模块的一致性和专业性，为后续实现具体的传输协议支持（如`aggligator-transport-udp`）奠定了坚实基础。
