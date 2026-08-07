//! 命令行子命令的展示逻辑。
//!
//! 与 `main` 的组装流程分开：这些只负责"把信息打给用户看"，不参与同步。

/// 打印本机可达地址及其性质，便于排查连通性。
///
/// 注意：这里描述的是地址**性质**而非优先级。优先级由对端按自身网络位置判定
/// ——同一个 192.168.x 地址，对同网段设备是"直连"，对异地设备则不可达；
/// 覆盖网地址则相反。故此处只如实说明每个地址的类型。
pub(crate) fn print_local_addrs(port: u16) {
    use clipsync_net::local::{local_candidates, local_networks};

    let cands = local_candidates(port);
    println!("本机可达地址（配对与连接时会告知对端）：");
    if cands.is_empty() {
        println!("  （未找到可用地址，请检查网络连接）");
        return;
    }
    for sa in &cands {
        println!("  {:<42} {}", sa.to_string(), describe_addr(sa.ip()));
    }

    let nets = local_networks();
    println!();
    println!(
        "本机网段：IPv4 {} 个、IPv6 {} 个",
        nets.v4.len(),
        nets.v6.len()
    );
    println!();
    println!("说明：覆盖网（Tailscale / ZeroTier / Netbird / WireGuard 等）的虚拟网卡地址");
    println!("      会自动出现在上表中，无需任何额外配置。对端连接时按");
    println!("      「同网段直连 → 覆盖网 → 公网」的顺序自动优选最快路径。");
}

/// 用平实语言描述一个地址的性质。
fn describe_addr(ip: std::net::IpAddr) -> &'static str {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            if o[0] == 100 && (64..128).contains(&o[1]) {
                "覆盖网段 (100.64/10 CGNAT，Tailscale 等常用)"
            } else if v4.is_private() {
                "局域网/私有网段"
            } else {
                "公网地址"
            }
        }
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments();
            if (seg[0] & 0xfe00) == 0xfc00 {
                "覆盖网段 (IPv6 ULA)"
            } else {
                "公网 IPv6"
            }
        }
    }
}

