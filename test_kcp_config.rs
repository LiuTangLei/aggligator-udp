//! Test KCP config
use tokio_kcp::{KcpConfig, KcpNoDelayConfig};

#[tokio::main]
async fn main() {
    let config = KcpConfig::default();
    println!("Default config: {:?}", config);
    
    let fastest = KcpNoDelayConfig::fastest();
    println!("Fastest config: {:?}", fastest);
    
    // 测试直接字段访问
    println!("NoDelay fields:");
    println!("  nodelay: {:?}", fastest.nodelay);
    println!("  interval: {:?}", fastest.interval);
    println!("  resend: {:?}", fastest.resend);
    println!("  nc: {:?}", fastest.nc);
}
