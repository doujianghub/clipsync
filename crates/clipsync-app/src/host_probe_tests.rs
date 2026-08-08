//! `host_probe` 的单元测试。

use super::*;

/// 从组网工具输出里捞地址，不依赖任何一家的输出格式。
///
/// 样本取自 `tailscale status` 的真实输出：一行一台，首列是地址。测试刻意
/// 混进版本号、端口号与主机名里的数字，确认它们不会被误认成地址。
#[test]
fn scrapes_overlay_addresses_without_parsing_json() {
    let sample = "\
100.88.88.22    wangs-macbook        user@ macOS   -
100.88.88.11    mac-mini             user@ macOS   active; direct 192.168.3.7:41641
100.115.98.83   ip-172-31-44-214     user@ linux   offline
# Health check: client version 1.62.0 is out of date
";
    let ips = scrape_ipv4(sample);

    assert!(ips.contains(&"100.88.88.22".parse().unwrap()));
    assert!(ips.contains(&"100.88.88.11".parse().unwrap()));
    assert!(
        ips.contains(&"100.115.98.83".parse().unwrap()),
        "跨网段的对端不能漏"
    );
    assert!(
        ips.contains(&"192.168.3.7".parse().unwrap()),
        "直连端点也是可达地址，捞上来无妨"
    );
    assert!(
        !ips.iter().any(|ip| ip.to_string() == "1.62.0"),
        "版本号不是地址"
    );
}

/// 公网地址与本机地址不该进候选：前者不是对端该在的地方，后者是自己。
#[test]
fn skips_addresses_that_cannot_be_a_peer() {
    assert!(!is_reachable_peer("127.0.0.1".parse().unwrap()));
    assert!(!is_reachable_peer("169.254.1.1".parse().unwrap()));
    assert!(!is_reachable_peer("0.0.0.0".parse().unwrap()));
    assert!(!is_reachable_peer("8.8.8.8".parse().unwrap()), "公网不探");

    assert!(is_reachable_peer("192.168.3.7".parse().unwrap()));
    assert!(
        is_reachable_peer("10.147.17.5".parse().unwrap()),
        "ZeroTier 常用段"
    );
    assert!(is_reachable_peer("100.88.88.11".parse().unwrap()), "CGNAT");
}

/// 网段枚举必须掐掉网络号、广播地址和本机自己。
///
/// 漏掉本机会让探测连上自己的配对监听（如果正好也在主持），拿到一个必然
/// 握不成手的连接；漏掉广播地址则会在某些系统上触发意料之外的行为。
#[test]
fn subnet_enumeration_excludes_network_broadcast_and_self() {
    // 直接验证枚举算法而不依赖本机网卡：/29 有 8 个地址，去掉网络号、
    // 广播和自己，应剩 5 个。
    let ip: Ipv4Addr = "192.168.3.3".parse().unwrap();
    let mask: Ipv4Addr = "255.255.255.248".parse().unwrap(); // /29
    let m = u32::from(mask);
    let base = u32::from(ip) & m;
    let bcast = base | !m;

    let hosts: Vec<Ipv4Addr> = ((base + 1)..bcast)
        .filter(|a| *a != u32::from(ip))
        .map(Ipv4Addr::from)
        .collect();

    assert_eq!(hosts.len(), 5);
    assert!(!hosts.contains(&Ipv4Addr::from(base)), "网络号不该在内");
    assert!(!hosts.contains(&Ipv4Addr::from(bcast)), "广播地址不该在内");
    assert!(!hosts.contains(&ip), "不该探自己");
}

/// 探测一个确定没有主持方的地址集，必须干脆返回空而不是卡住。
///
/// 这条测试在开发机上曾**失败**，暴露出一个真实缺陷：那台机器开着 TUN 模式
/// 代理，整张路由表被接管，连 192.0.2.1 都"连得上"。只认连接成功的话，探测
/// 会把第一个候选当成主持方。修法见 `speaks_pairing`。
#[test]
fn probing_dead_addresses_returns_quickly() {
    // 192.0.2.0/24 是 RFC 5737 保留的文档用网段，不会有人真的监听。
    let cands: Vec<SocketAddr> = (1..=8)
        .map(|i| format!("192.0.2.{i}:47685").parse().unwrap())
        .collect();

    let started = std::time::Instant::now();
    let found = probe(cands);

    assert!(found.is_empty());
    // 8 个地址、并发探测，理应在一个超时周期多一点内结束。
    assert!(
        started.elapsed() < (PROBE_TIMEOUT + GREETING_TIMEOUT) * 3,
        "并发探测不该退化成串行等待，实际耗时 {:?}",
        started.elapsed()
    );
}

/// 真的有人监听时，探测必须找到它，并且返回**可用的连接**而不只是地址。
#[test]
fn probing_finds_a_listening_host_and_hands_back_the_connection() {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // 照主持方的样子先发一帧：4 字节大端长度 + 负载。探测器认的就是这个。
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.write_all(&2u32.to_be_bytes()).unwrap();
        s.write_all(b"hi").unwrap();
    });

    // 直接喂给 probe——它不关心地址是怎么来的，这正是"一个探测器 + 若干
    // 来源"的意义。
    let mut found = probe(vec![addr]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, addr);

    // 返回的连接必须能直接用，且**第一帧还在**——探测是 peek 而不是 read，
    // 否则这一探就把对方的第一帧吃掉，后面的握手必然失败。
    let mut buf = [0u8; 6];
    found[0].1.read_exact(&mut buf).unwrap();
    assert_eq!(&buf[..4], &2u32.to_be_bytes());
    assert_eq!(&buf[4..], b"hi");

    server.join().unwrap();
}

/// 只连上、不说话的对面不算主持方——TUN 模式代理就是这样。
#[test]
fn a_silent_listener_is_not_a_pairing_host() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        std::thread::sleep(GREETING_TIMEOUT * 2);
        drop(s);
    });

    assert!(probe(vec![addr]).is_empty(), "不开口的不能当成主持方");
    server.join().unwrap();
}

/// 发别的协议的服务也不算——帧头前两字节必须是 0。
#[test]
fn a_service_speaking_another_protocol_is_not_a_pairing_host() {
    use std::io::Write;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\n");
    });

    assert!(probe(vec![addr]).is_empty(), "HTTP 服务不是主持方");
    server.join().unwrap();
}

/// 手动诊断：打印本机各来源实际产出的候选，确认发现链在真实网络上是活的。
///
/// 跑法：`cargo test -p clipsync-app --bin clipsync -- --ignored --nocapture candidate_sources`
///
/// 判据：网段那一路应给出本机所在 /24 的地址数；接着覆盖网那一路应列出
/// 组网工具报告的对端。两路都空，说明这台机器上只能手输地址。
#[test]
#[ignore = "结果取决于本机网络环境，只用于人工核对"]
fn manual_show_candidate_sources() {
    let subnets = subnet_candidates(47_685);
    println!("网段枚举：{} 个候选", subnets.len());
    if let (Some(a), Some(b)) = (subnets.first(), subnets.last()) {
        println!("  范围 {a} … {b}");
    }

    let overlay = overlay_tool_candidates(47_685);
    println!("组网工具：{} 个对端", overlay.len());
    for a in &overlay {
        println!("  {a}");
    }

    // 整轮耗时——用户点完「输入配对码」要等的就是这段。
    let t0 = std::time::Instant::now();
    let found = find_hosts(47_685);
    println!("整轮探测耗时 {:?}，命中 {} 台", t0.elapsed(), found.len());
}

/// 覆盖网邻域兜底只针对**不可枚举**的接口，且要掐掉自己。
///
/// 回归自实机故障：Mac mini 上 Tailscale 通着，却因为 `tailscale` 命令不在
/// 路径表里而取到 0 个对端；两台机器又不在同一局域网，于是彻底找不到人。
/// 路径表永远列不全，所以要有一条不认厂商的兜底。
#[test]
fn overlay_neighborhood_covers_the_slash_24_around_own_address() {
    let cands = overlay_neighborhood(47_685);
    let own = own_ipv4();

    for a in &cands {
        let std::net::IpAddr::V4(ip) = a.ip() else {
            panic!("只该产出 IPv4")
        };
        assert!(!own.contains(&ip), "不该探自己：{ip}");
        assert_ne!(ip.octets()[3], 0, "网络号不该在内");
        assert_ne!(ip.octets()[3], 255, "广播地址不该在内");
        assert!(is_reachable_peer(ip), "只探私有/CGNAT 段");
    }

    // 本机若有 /32 的覆盖网接口（Tailscale 的 utun 就是），必须给出邻域；
    // 没有这类接口时为空也是对的——那说明所有接口都有真实网段，
    // subnet_candidates 已经覆盖。
    let has_overlay_iface = clipsync_net::local::local_networks()
        .v4
        .into_iter()
        .any(|(ip, mask)| u32::from(mask).count_ones() > 30 && is_reachable_peer(ip));
    assert_eq!(
        has_overlay_iface,
        !cands.is_empty(),
        "有不可枚举的覆盖网接口就该有邻域候选，反之为空"
    );
}
