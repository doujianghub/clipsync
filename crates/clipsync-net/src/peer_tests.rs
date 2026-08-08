//! `peer` 的单元测试。
//!
//! 单独成文件只为让 `peer.rs` 保持在项目约定的行数以内；
//! 它仍是 `peer` 的子模块（由 `#[path]` 引入），`use super::*`
//! 照常可用，与写在原文件里没有区别。

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
    assert_eq!(
        classify(ip("127.0.0.1"), &local()),
        Some(AddrClass::LanDirect)
    );
}

/// 不同网段的私有地址属于覆盖网（ZeroTier/WireGuard 等常见网段）。
#[test]
fn other_private_subnet_is_overlay() {
    assert_eq!(
        classify(ip("10.147.20.3"), &local()),
        Some(AddrClass::Overlay)
    );
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
    assert_eq!(
        classify(ip("100.20.1.1"), &local()),
        Some(AddrClass::Public)
    );
}

#[test]
fn public_ipv4_is_public() {
    assert_eq!(
        classify(ip("203.0.113.9"), &local()),
        Some(AddrClass::Public)
    );
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
    assert_eq!(
        classify(ip("2001:db8:0:2::99"), &l),
        Some(AddrClass::Public)
    );
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
