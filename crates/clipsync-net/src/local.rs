//! 本机网络地址枚举。
//!
//! **通用识别覆盖网的关键**：直接枚举本机所有网卡地址，而不去识别具体的组网
//! 产品。Tailscale、ZeroTier、Netbird、Nebula、WireGuard 等都会在系统上创建
//! 虚拟网卡并分配 IP，因此它们的地址会自动出现在枚举结果中，无需为每种产品
//! 编写适配代码。新出现的组网方案也能自动支持。
//!
//! 两个用途：
//!   1. [`local_networks`]：得到本机所在网段，供 [`crate::peer::classify`] 判断
//!      对端地址是否"同网段直连"。
//!   2. [`local_candidates`]：得到本机可被对端连接的地址列表���用于局域网信标
//!      广播、配对时交换、以及连接后通过加密通道告知对端。

use std::net::{IpAddr, SocketAddr};

use crate::peer::{classify, AddrClass, LocalNetworks};

/// 枚举本机各网卡的地址与网段（用于判断"同网段"）。
///
/// 枚举失败时返回空集合——此时 [`classify`] 会把所有私有地址归为
/// `Overlay` 而非 `LanDirect`，仍可正常连接，只是优先级判断略保守。
pub fn local_networks() -> LocalNetworks {
    let mut out = LocalNetworks::default();
    let ifaces = match if_addrs::get_if_addrs() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("枚举本机网卡失败（将退化为保守的地址分类）: {e}");
            return out;
        }
    };
    for iface in ifaces {
        match iface.addr {
            if_addrs::IfAddr::V4(v4) => out.v4.push((v4.ip, v4.netmask)),
            if_addrs::IfAddr::V6(v6) => out.v6.push((v6.ip, v6.prefixlen)),
        }
    }
    out
}

/// 本机可供对端连接的候选地址（附上监听端口）。
///
/// 过滤掉回环与链路本地地址（对端无法使用），其余一律保留——不区分物理网卡
/// 还是覆盖网虚拟网卡，由对端按 [`classify`] 自行判断优先级。
pub fn local_candidates(port: u16) -> Vec<SocketAddr> {
    let ifaces = match if_addrs::get_if_addrs() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("枚举本机地址失败: {e}");
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for iface in ifaces {
        let ip = iface.ip();
        if !is_advertisable(ip) {
            continue;
        }
        let sa = SocketAddr::new(ip, port);
        if !out.contains(&sa) {
            out.push(sa);
        }
    }
    out
}

/// 该地址是否值得告诉对端。
fn is_advertisable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified() && !v4.is_multicast()
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            let link_local = (seg[0] & 0xffc0) == 0xfe80;
            !v6.is_loopback() && !link_local && !v6.is_unspecified() && !v6.is_multicast()
        }
    }
}

/// 用本机网段信息给一批对端地址分类，产出候选。
///
/// 便捷函数：连接管理层拿到一组对端地址（来自信标/配对/对端通告）后，
/// 用它统一分类并过滤掉不可用地址。
pub fn classify_all(
    addrs: impl IntoIterator<Item = SocketAddr>,
    local: &LocalNetworks,
) -> Vec<(SocketAddr, AddrClass)> {
    addrs
        .into_iter()
        .filter_map(|sa| classify(sa.ip(), local).map(|c| (sa, c)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 枚举本机地址应能正常执行（具体结果依赖运行环境，故只做基本约束）。
    #[test]
    fn enumerating_local_addresses_works() {
        let nets = local_networks();
        // 任何机器至少应有回环网卡；若枚举成功则 v4 非空。
        // 枚举失败时返回空集合也是允许的（不应 panic）。
        let _ = nets.v4.len();

        let cands = local_candidates(47684);
        // 广播候选中不应包含回环或链路本地地址。
        for sa in &cands {
            assert!(is_advertisable(sa.ip()), "不应广播不可用地址: {sa}");
            assert_eq!(sa.port(), 47684);
        }
    }

    #[test]
    fn loopback_and_link_local_are_not_advertisable() {
        assert!(!is_advertisable("127.0.0.1".parse().unwrap()));
        assert!(!is_advertisable("169.254.1.1".parse().unwrap()));
        assert!(!is_advertisable("::1".parse().unwrap()));
        assert!(!is_advertisable("fe80::1".parse().unwrap()));
    }

    #[test]
    fn normal_addresses_are_advertisable() {
        assert!(is_advertisable("192.168.1.10".parse().unwrap()));
        assert!(is_advertisable("100.101.102.103".parse().unwrap())); // 覆盖网
        assert!(is_advertisable("10.147.20.3".parse().unwrap())); // 覆盖网
        assert!(is_advertisable("203.0.113.9".parse().unwrap())); // 公网
    }

    #[test]
    fn classify_all_filters_unusable() {
        let local = LocalNetworks::default();
        let addrs = vec![
            "192.168.1.5:1".parse().unwrap(),
            "169.254.1.1:1".parse().unwrap(), // 应被过滤
            "100.64.0.1:1".parse().unwrap(),
        ];
        let out = classify_all(addrs, &local);
        assert_eq!(out.len(), 2, "链路本地地址应被过滤");
    }
}
