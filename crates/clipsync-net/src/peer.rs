//! 候选地址管理与路径优选（通用设计，不绑定任何具体组网产品）。
//!
//! **设计要点：不认产品，只认地址。**
//!
//! Tailscale、ZeroTier、Netbird、Nebula、WireGuard、乃至公网端口转发，本质都是
//! "给设备一个可路由的 IP"。因此本模块不针对任何产品做适配，而是把所有可能的
//! 地址统一收集为候选，按**网络拓扑距离**分类定优先级：
//!
//!   1. [`AddrClass::LanDirect`] —— 与本机处于同一网段（或本机回环）。延迟最低、
//!      带宽最高，最优先。
//!   2. [`AddrClass::Overlay`] —— 私有网段但不同网段：各类覆盖网/VPN 虚拟网卡
//!      （Tailscale 的 100.64/10、ZeroTier 的 10.x、WireGuard 的自定义网段…）。
//!   3. [`AddrClass::Public`] —— 公网地址（端口转发、DDNS、云主机）。
//!
//! 候选地址的**来源**（[`AddrSource`]）只用于诊断与过期策略，不影响优先级——
//! 无论来自局域网信标、配对时交换、还是对端通过加密通道告知，同类地址一视同仁。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use clipsync_core::DeviceId;
use serde::{Deserialize, Serialize};

/// 地址类别：决定连接优先级。判别值越小越优先。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AddrClass {
    /// 同网段直连（含本机回环）——最低延迟。
    LanDirect = 0,
    /// 覆盖网/VPN/异网段私有地址——跨地域兜底。
    Overlay = 1,
    /// 公网地址——最后尝试。
    Public = 2,
}

/// 地址来源：用于诊断与过期策略，**不参与优先级排序**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AddrSource {
    /// 局域网 UDP 信标发现（动态，会过期）。
    Beacon,
    /// 已连接对端通过加密通道告知（动态刷新）。
    Peer,
    /// 配对时交换并持久化。
    Pairing,
    /// 用户手动配置。
    Manual,
}

/// 一个候选地址。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub addr: SocketAddr,
    pub class: AddrClass,
    pub source: AddrSource,
    /// 曾经成功连接过：同类地址中优先重试，加快重连。
    #[serde(default)]
    pub known_good: bool,
}

impl Candidate {
    pub fn new(addr: SocketAddr, class: AddrClass, source: AddrSource) -> Self {
        Self {
            addr,
            class,
            source,
            known_good: false,
        }
    }
}

/// 本机各网卡的地址与掩码，用于判断"是否同网段"。
#[derive(Debug, Clone, Default)]
pub struct LocalNetworks {
    /// (本机 IPv4, 子网掩码)
    pub v4: Vec<(Ipv4Addr, Ipv4Addr)>,
    /// (本机 IPv6, 前缀长度)
    pub v6: Vec<(Ipv6Addr, u8)>,
}

impl LocalNetworks {
    fn same_subnet_v4(&self, ip: Ipv4Addr) -> bool {
        let ip = u32::from(ip);
        self.v4.iter().any(|(local, mask)| {
            let m = u32::from(*mask);
            // 掩码为 0 时视为不构成有效网段判断，避免误判所有地址为同网段。
            m != 0 && (u32::from(*local) & m) == (ip & m)
        })
    }

    fn same_prefix_v6(&self, ip: Ipv6Addr) -> bool {
        self.v6.iter().any(|(local, prefix_len)| {
            let n = (*prefix_len).min(128) as usize;
            if n == 0 {
                return false;
            }
            let a = local.octets();
            let b = ip.octets();
            let full = n / 8;
            let rem = n % 8;
            if a[..full] != b[..full] {
                return false;
            }
            if rem == 0 {
                return true;
            }
            let mask = 0xffu8 << (8 - rem);
            (a[full] & mask) == (b[full] & mask)
        })
    }
}

/// 判断一个 IP 属于哪个类别；返回 `None` 表示该地址不适合作为候选（应跳过）。
///
/// 这是"通用识别"的核心：仅凭地址所在网段判断拓扑距离，不需要知道它由哪个
/// 组网产品分配。
pub fn classify(ip: IpAddr, local: &LocalNetworks) -> Option<AddrClass> {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast() {
                return None;
            }
            // 169.254/16 链路本地：自动配置地址，通常不可靠。
            if v4.is_link_local() {
                return None;
            }
            if v4.is_loopback() {
                return Some(AddrClass::LanDirect);
            }
            if local.same_subnet_v4(v4) {
                return Some(AddrClass::LanDirect);
            }
            // 100.64/10（运营商级 NAT 段）：Tailscale 等覆盖网常用。
            let o = v4.octets();
            let is_cgnat = o[0] == 100 && (64..128).contains(&o[1]);
            if v4.is_private() || is_cgnat {
                return Some(AddrClass::Overlay);
            }
            Some(AddrClass::Public)
        }
        IpAddr::V6(v6) => {
            if v6.is_unspecified() || v6.is_multicast() {
                return None;
            }
            // fe80::/10 链路本地：需带 scope id，跨主机使用不可靠。
            let seg = v6.segments();
            if (seg[0] & 0xffc0) == 0xfe80 {
                return None;
            }
            if v6.is_loopback() {
                return Some(AddrClass::LanDirect);
            }
            if local.same_prefix_v6(v6) {
                return Some(AddrClass::LanDirect);
            }
            // fc00::/7 唯一本地地址（ULA）：各类覆盖网常用（含 Tailscale 的 fd7a::/48）。
            if (seg[0] & 0xfe00) == 0xfc00 {
                return Some(AddrClass::Overlay);
            }
            Some(AddrClass::Public)
        }
    }
}

/// 某设备的候选地址集合，按优先级给出连接顺序。
#[derive(Debug, Clone, Default)]
pub struct PeerAddresses {
    candidates: Vec<Candidate>,
}

impl PeerAddresses {
    pub fn new() -> Self {
        Self::default()
    }

    /// 加入或更新一个候选（按 addr 去重）。
    ///
    /// 已存在时更新其类别与来源，但**保留** `known_good` 标记（历史成功经验
    /// 不应被一次新发现清除）。
    pub fn upsert(&mut self, cand: Candidate) {
        if let Some(e) = self.candidates.iter_mut().find(|c| c.addr == cand.addr) {
            e.class = cand.class;
            e.source = cand.source;
            e.known_good |= cand.known_good;
        } else {
            self.candidates.push(cand);
        }
    }

    /// 批量合并候选。
    pub fn extend(&mut self, cands: impl IntoIterator<Item = Candidate>) {
        for c in cands {
            self.upsert(c);
        }
    }

    /// 标记某地址连接成功过（下次同类优先重试）。
    pub fn mark_good(&mut self, addr: &SocketAddr) {
        if let Some(e) = self.candidates.iter_mut().find(|c| &c.addr == addr) {
            e.known_good = true;
        }
    }

    /// 移除某来源的全部候选（如信标超时后清除局域网候选）。
    pub fn clear_source(&mut self, source: AddrSource) {
        self.candidates.retain(|c| c.source != source);
    }

    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    /// 按优先级返回应依次尝试的地址：
    /// 先按类别（LanDirect → Overlay → Public），同类中曾成功者优先，
    /// 其余保持插入顺序（稳定排序）。
    pub fn connect_order(&self) -> Vec<Candidate> {
        let mut ordered = self.candidates.clone();
        ordered.sort_by_key(|c| (c.class, !c.known_good));
        ordered
    }

    /// 最优候选。
    pub fn best(&self) -> Option<Candidate> {
        self.connect_order().into_iter().next()
    }

    /// 全部候选（不排序），用于持久化/上报。
    pub fn all(&self) -> &[Candidate] {
        &self.candidates
    }
}

/// 已配对设备及其候选地址（内存中的地址簿条目）。
#[derive(Debug, Clone)]
pub struct PeerEntry {
    pub device: DeviceId,
    pub addresses: PeerAddresses,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// 本机处于 192.168.1.0/24。
    fn local() -> LocalNetworks {
        LocalNetworks {
            v4: vec![(
                "192.168.1.10".parse().unwrap(),
                "255.255.255.0".parse().unwrap(),
            )],
            v6: vec![],
        }
    }

    #[test]
    fn same_subnet_is_lan_direct() {
        assert_eq!(
            classify(ip("192.168.1.55"), &local()),
            Some(AddrClass::LanDirect)
        );
    }

    #[test]
    fn loopback_is_lan_direct() {
        assert_eq!(classify(ip("127.0.0.1"), &local()), Some(AddrClass::LanDirect));
    }

    /// 不同网段的私有地址属于覆盖网（ZeroTier/WireGuard 等常见网段）。
    #[test]
    fn other_private_subnet_is_overlay() {
        assert_eq!(classify(ip("10.147.20.3"), &local()), Some(AddrClass::Overlay));
        assert_eq!(
            classify(ip("172.16.5.9"), &local()),
            Some(AddrClass::Overlay)
        );
        // 同为 192.168 但不同网段，仍属覆盖/异网段。
        assert_eq!(
            classify(ip("192.168.99.7"), &local()),
            Some(AddrClass::Overlay)
        );
    }

    /// 100.64/10 运营商级 NAT 段（Tailscale 等使用）属于覆盖网。
    #[test]
    fn cgnat_range_is_overlay() {
        assert_eq!(
            classify(ip("100.101.102.103"), &local()),
            Some(AddrClass::Overlay)
        );
        // 100.x 但不在 64..128 区间的是公网地址。
        assert_eq!(classify(ip("100.20.1.1"), &local()), Some(AddrClass::Public));
    }

    #[test]
    fn public_ipv4_is_public() {
        assert_eq!(classify(ip("203.0.113.9"), &local()), Some(AddrClass::Public));
    }

    #[test]
    fn link_local_and_multicast_are_skipped() {
        assert_eq!(classify(ip("169.254.1.1"), &local()), None);
        assert_eq!(classify(ip("224.0.0.251"), &local()), None);
        assert_eq!(classify(ip("0.0.0.0"), &local()), None);
    }

    #[test]
    fn ipv6_ula_is_overlay_and_link_local_skipped() {
        let l = LocalNetworks::default();
        // Tailscale 的 IPv6 段 fd7a::/48 属于 ULA。
        assert_eq!(
            classify(ip("fd7a:115c:a1e0::1"), &l),
            Some(AddrClass::Overlay)
        );
        assert_eq!(classify(ip("fe80::1"), &l), None);
        assert_eq!(classify(ip("::1"), &l), Some(AddrClass::LanDirect));
        assert_eq!(classify(ip("2001:db8::1"), &l), Some(AddrClass::Public));
    }

    #[test]
    fn ipv6_same_prefix_is_lan_direct() {
        let l = LocalNetworks {
            v4: vec![],
            v6: vec![("2001:db8:0:1::5".parse().unwrap(), 64)],
        };
        assert_eq!(
            classify(ip("2001:db8:0:1::99"), &l),
            Some(AddrClass::LanDirect)
        );
        assert_eq!(classify(ip("2001:db8:0:2::99"), &l), Some(AddrClass::Public));
    }

    #[test]
    fn connect_order_prefers_lan_then_overlay_then_public() {
        let mut p = PeerAddresses::new();
        // 故意乱序插入。
        p.upsert(Candidate::new(
            sa("203.0.113.9:47684"),
            AddrClass::Public,
            AddrSource::Manual,
        ));
        p.upsert(Candidate::new(
            sa("100.101.102.103:47684"),
            AddrClass::Overlay,
            AddrSource::Peer,
        ));
        p.upsert(Candidate::new(
            sa("192.168.1.55:47684"),
            AddrClass::LanDirect,
            AddrSource::Beacon,
        ));

        let order = p.connect_order();
        assert_eq!(order[0].class, AddrClass::LanDirect);
        assert_eq!(order[1].class, AddrClass::Overlay);
        assert_eq!(order[2].class, AddrClass::Public);
    }

    #[test]
    fn known_good_wins_within_same_class() {
        let mut p = PeerAddresses::new();
        p.upsert(Candidate::new(
            sa("100.0.0.1:1"),
            AddrClass::Overlay,
            AddrSource::Peer,
        ));
        p.upsert(Candidate::new(
            sa("100.0.0.2:1"),
            AddrClass::Overlay,
            AddrSource::Peer,
        ));
        p.mark_good(&sa("100.0.0.2:1"));

        let order = p.connect_order();
        assert_eq!(order[0].addr, sa("100.0.0.2:1"), "曾成功的地址应优先");
    }

    #[test]
    fn upsert_dedups_and_preserves_known_good() {
        let mut p = PeerAddresses::new();
        p.upsert(Candidate::new(
            sa("192.168.1.5:47684"),
            AddrClass::LanDirect,
            AddrSource::Beacon,
        ));
        p.mark_good(&sa("192.168.1.5:47684"));
        // 再次发现同一地址（来源不同），不应清除 known_good。
        p.upsert(Candidate::new(
            sa("192.168.1.5:47684"),
            AddrClass::LanDirect,
            AddrSource::Peer,
        ));

        assert_eq!(p.len(), 1);
        assert!(p.all()[0].known_good);
        assert_eq!(p.all()[0].source, AddrSource::Peer);
    }

    #[test]
    fn clear_source_removes_only_that_source() {
        let mut p = PeerAddresses::new();
        p.upsert(Candidate::new(
            sa("192.168.1.5:1"),
            AddrClass::LanDirect,
            AddrSource::Beacon,
        ));
        p.upsert(Candidate::new(
            sa("100.0.0.1:1"),
            AddrClass::Overlay,
            AddrSource::Pairing,
        ));
        p.clear_source(AddrSource::Beacon);
        assert_eq!(p.len(), 1);
        assert_eq!(p.all()[0].source, AddrSource::Pairing);
    }

    #[test]
    fn empty_has_no_best() {
        let p = PeerAddresses::new();
        assert!(p.is_empty());
        assert!(p.best().is_none());
    }
}
