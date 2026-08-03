//! 配对命令行流程。
//!
//! 配对需要一条临时 TCP 通道让两台设备跑 PAKE 握手。为"极简配置"，采用：
//!   - **接受方**（`clipsync pair --host`）：生成配对码并显示，在配对端口监听，
//!     等对方连入后跑 `run_pairing`，成功则保存配对记录。
//!   - **发起方**（`clipsync pair <ip> <code>`）：连接对方配对端口，输入配对码，
//!     跑 `run_pairing`，成功则保存配对记录。
//!
//! 二者对称调用 `clipsync_net::pairing_handshake::run_pairing`。配对端口与同步
//! 端口分开，避免与常规同步连接混淆。M3 起可用 mDNS 免去手填 IP。

use std::net::{TcpListener, TcpStream};
use std::path::Path;

use anyhow::{Context, Result};
use clipsync_net::crypto::StaticIdentity;
use clipsync_net::pairing::PairingCode;
use clipsync_net::pairing_handshake::{run_pairing, LocalPairingInfo};

use crate::config;

/// 配对使用的 TCP 端口（与同步端口区分）。
pub const PAIRING_PORT: u16 = 47_685;

/// 作为接受方主持配对：生成并显示配对码，等待对方连入。
pub fn host(
    dir: &Path,
    identity: &StaticIdentity,
    device_name: &str,
    sync_port: u16,
) -> Result<()> {
    let code = PairingCode::generate();
    println!();
    println!("  配对码： {}", code);
    println!();
    println!("  在另一台设备上运行：");
    println!("      clipsync pair <本机IP> {}", code);
    println!();
    println!("  正在等待对方连接（端口 {PAIRING_PORT}）…");

    let listener = TcpListener::bind(("0.0.0.0", PAIRING_PORT))
        .with_context(|| format!("监听配对端口 {PAIRING_PORT} 失败"))?;
    let (mut stream, addr) = listener.accept().context("接受配对连接失败")?;
    println!("  对方已连接：{addr}，正在协商…");

    let local = local_info(identity, device_name, sync_port);
    let record = run_pairing(&mut stream, &code, &local).context("配对握手失败")?;

    config::upsert_pairing(dir, record.clone())?;
    println!("  ✓ 配对成功：{} ({})", record.name, record.device);
    print_learned_addrs(&record);
    Ok(())
}

/// 作为发起方连接对方完成配对。
pub fn join(
    dir: &Path,
    identity: &StaticIdentity,
    device_name: &str,
    host_ip: &str,
    code_str: &str,
    sync_port: u16,
) -> Result<()> {
    let code =
        PairingCode::parse(code_str).with_context(|| format!("配对码格式非法: {code_str}"))?;

    let addr = format!("{host_ip}:{PAIRING_PORT}");
    println!("  正在连接 {addr} …");
    let mut stream = TcpStream::connect(&addr).with_context(|| format!("连接 {addr} 失败"))?;
    println!("  已连接，正在协商…");

    let local = local_info(identity, device_name, sync_port);
    let record = run_pairing(&mut stream, &code, &local).context("配对握手失败")?;

    config::upsert_pairing(dir, record.clone())?;
    println!("  ✓ 配对成功：{} ({})", record.name, record.device);
    print_learned_addrs(&record);
    Ok(())
}

/// 展示配对时学到的对端地址，让用户直观看到可用路径。
fn print_learned_addrs(record: &clipsync_net::pairing::PairingRecord) {
    if record.addrs.is_empty() {
        return;
    }
    println!("  已记录对方 {} 个可达地址：", record.addrs.len());
    for a in &record.addrs {
        println!("      {a}");
    }
}

/// 本机在配对中提供的信息：身份 + 全部可达地址。
///
/// 地址由网卡枚举得来，因此 Tailscale/ZeroTier/WireGuard 等覆盖网的虚拟网卡
/// 地址会自动包含在内——配对完成后即使离开局域网也能直接连通。
fn local_info(identity: &StaticIdentity, device_name: &str, sync_port: u16) -> LocalPairingInfo {
    LocalPairingInfo {
        device_id: identity.device_id().to_string(),
        name: device_name.to_string(),
        static_public_key: identity.public_key.clone(),
        addrs: clipsync_net::local::local_candidates(sync_port),
    }
}
