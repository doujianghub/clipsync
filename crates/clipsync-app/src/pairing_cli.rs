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
///
/// 同时向局域网宣告"我在等待配对"，使对方**无需手输 IP**——直接
/// `clipsync pair <配对码>` 即可。跨网络时才需要用到下方打印的地址。
pub fn host(
    dir: &Path,
    identity: &StaticIdentity,
    device_name: &str,
    sync_port: u16,
) -> Result<()> {
    let code = PairingCode::generate();

    // 向局域网宣告等待配对（失败不致命，退化为需手输 IP）。
    let announcing =
        clipsync_net::discovery::spawn_pairing_announcer(device_name.to_string(), PAIRING_PORT)
            .is_ok();

    println!();
    println!("  配对码： {}", code);
    println!();
    if announcing {
        println!("  在另一台设备上运行（同一局域网内，无需输入 IP）：");
        println!("      clipsync pair {}", code);
        println!();
    }
    print_manual_hint(&code, sync_port);
    println!("  正在等待对方连接…");

    let listener = TcpListener::bind(("0.0.0.0", PAIRING_PORT))
        .with_context(|| format!("监听配对端口 {PAIRING_PORT} 失败"))?;

    // 循环接受，直到配对成功——避免一次失败尝试（如误连或配对码输错）
    // 就让主持方退出，用户还得重来一遍。
    loop {
        let (mut stream, addr) = listener.accept().context("接受配对连接失败")?;
        println!("  对方已连接：{addr}，正在协商…");

        let local = local_info(identity, device_name, sync_port);
        match run_pairing(&mut stream, &code, &local) {
            Ok(record) => {
                config::upsert_pairing(dir, record.clone())?;
                println!("  ✓ 配对成功：{} ({})", record.name, record.device);
                print_learned_addrs(&record);
                return Ok(());
            }
            Err(e) => {
                println!("  ✗ 本次配对未成功：{e:#}");
                println!("  仍在等待（配对码不变，可重试）…");
            }
        }
    }
}

/// 打印跨网络场景下需要手动指定的地址。
fn print_manual_hint(code: &PairingCode, sync_port: u16) {
    let addrs = clipsync_net::local::local_candidates(sync_port);
    if addrs.is_empty() {
        return;
    }
    println!("  若两台设备不在同一局域网，在对方设备上运行：");
    for sa in &addrs {
        println!("      clipsync pair {} {}", fmt_host(sa), code);
    }
    println!();
}

/// 把 SocketAddr 的主机部分格式化为可直接用于命令行的形式。
fn fmt_host(sa: &std::net::SocketAddr) -> String {
    match sa.ip() {
        // IPv6 需要方括号，避免与端口分隔符混淆。
        std::net::IpAddr::V6(v6) => format!("[{v6}]"),
        std::net::IpAddr::V4(v4) => v4.to_string(),
    }
}

/// 作为发起方完成配对。
///
/// `host_ip` 为 `None` 时在局域网中自动发现等待配对的设备。
pub fn join(
    dir: &Path,
    identity: &StaticIdentity,
    device_name: &str,
    host_ip: Option<&str>,
    code_str: &str,
    sync_port: u16,
) -> Result<()> {
    let code =
        PairingCode::parse(code_str).with_context(|| format!("配对码格式非法: {code_str}"))?;

    let addr = match host_ip {
        Some(ip) => format!("{ip}:{PAIRING_PORT}"),
        None => discover_host()?,
    };

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

/// 在局域网中查找正在等待配对的设备，返回其地址。
fn discover_host() -> Result<String> {
    use clipsync_net::discovery::discover_pairing_hosts;

    println!("  正在局域网中查找等待配对的设备…");
    let hosts = discover_pairing_hosts(std::time::Duration::from_secs(6))
        .context("局域网配对发现失败")?;

    match hosts.len() {
        0 => anyhow::bail!(
            "未在局域网中找到等待配对的设备。\n  \
             请确认对方已运行 `clipsync pair --host`；\n  \
             若两台设备不在同一局域网，请改用：clipsync pair <对方IP> <配对码>"
        ),
        1 => {
            let h = &hosts[0];
            println!("  找到设备：{}（{}）", h.device_name, h.addr.ip());
            Ok(h.addr.to_string())
        }
        _ => {
            // 多台同时在等待配对：让用户明确指定，避免连错设备。
            let mut msg = String::from("局域网中有多台设备在等待配对，请指定其一：\n");
            for h in &hosts {
                msg.push_str(&format!(
                    "      clipsync pair {} <配对码>   # {}\n",
                    h.addr.ip(),
                    h.device_name
                ));
            }
            anyhow::bail!(msg)
        }
    }
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
