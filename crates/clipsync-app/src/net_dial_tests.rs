//! `net_dial` 的单元测试。

use super::*;

/// **不拿真实地址测**：这台开发机上装着 TUN 模式代理，连 RFC 5737 的
/// 文档用网段都"连得上"，用它当死地址的断言必然失败（`host_probe` 栽过
/// 同一个坑）。注入一个假的连接操作，结论只取决于代码本身。
///
/// 端口号当作剧本：
///   47681 立刻失败
///   47682 睡 2 秒后成功        —— 慢得超出宽限期
///   47683 睡 50ms 后成功       —— 快
///   47684 睡 2 秒后失败        —— 模拟跨网段地址走满超时
///   47685 睡 150ms 后成功      —— 慢，但仍在宽限期内
fn fake_connect(a: SocketAddr) -> Option<u16> {
    match a.port() {
        47682 => {
            std::thread::sleep(Duration::from_secs(2));
            Some(47682)
        }
        47683 => {
            std::thread::sleep(Duration::from_millis(50));
            Some(47683)
        }
        47684 => {
            std::thread::sleep(Duration::from_secs(2));
            None
        }
        47685 => {
            std::thread::sleep(Duration::from_millis(150));
            Some(47685)
        }
        _ => None,
    }
}

fn addrs(ports: &[u16]) -> Vec<SocketAddr> {
    ports
        .iter()
        .map(|p| format!("192.0.2.1:{p}").parse().unwrap())
        .collect()
}

/// 一轮拨号的墙上时间必须是**一个**超时窗口，而不是"候选数 × 超时"。
///
/// 回归自实机：启动后两台在线设备分别等了 10 秒和 29 秒才连上。地址簿里
/// 一台有 5 个候选、一台有 3 个，串行逐个 `connect_timeout(3s)` 试下来，
/// 光等超时就要 15 秒和 9 秒。
#[test]
fn candidates_are_raced_not_tried_one_by_one() {
    // 六个"立刻失败"混一个"50ms 后成功"，串行也快——所以这里用会睡的那个
    // 当第一顺位，验的是"其余候选不会各自累加"。
    let list = addrs(&[47682, 47681, 47681, 47681, 47681, 47681]);
    let started = Instant::now();
    let got = race_by_priority(&list, fake_connect);
    let took = started.elapsed();

    assert_eq!(got, Some((0, 47682)));
    assert!(
        took < Duration::from_millis(2600),
        "应与最慢的那一个同量级（2 秒），而不是逐个累加，实际 {took:?}"
    );
}

/// **答案不可能更好就立刻返回**，不等剩下的跑完。
///
/// 这是"启动 3 秒"那个问题的直接修复：最优地址 50ms 就连上了，却因为要等
/// 另外几个走满 `DIAL_TIMEOUT` 而白白拖到 3 秒。
#[test]
fn a_fast_top_priority_hit_does_not_wait_for_the_stragglers() {
    // 第一顺位 50ms 成功，后面跟一个要睡 2 秒的。
    let list = addrs(&[47683, 47682, 47682]);
    let started = Instant::now();
    let got = race_by_priority(&list, fake_connect);
    let took = started.elapsed();

    assert_eq!(got, Some((0, 47683)));
    assert!(
        // 上限放到 1500ms 是为了容忍共享 CI runner 的调度抖动，同时仍小于
        // 慢候选的 2000ms——真退化成"等落后者"的话照样会被抓住。
        took < Duration::from_millis(1500),
        "最优候选已定就该马上走，实际等了 {took:?}"
    );
}

/// 优先级要压过速度——**在宽限期以内**。
///
/// TCP 建连的快慢跟后续吞吐没什么关系：同网段直连比覆盖网快一个数量级，
/// 让"先连上的赢"会经常选中更差的路。所以更优的那条慢一点也该等。
#[test]
fn priority_beats_speed_within_the_grace_window() {
    // 下标 0 慢（150ms）但优先级最高，下标 1 快（50ms）。差距在 300ms 以内。
    let list = addrs(&[47685, 47683]);
    let got = race_by_priority(&list, fake_connect);
    assert_eq!(got, Some((0, 47685)), "该等最优的那条");
}

/// 但不能无限等下去：高优先级迟迟无果时，用手里已经连上的那条。
///
/// 回归自实机日志——本机搬到了 192.168.3.x，地址簿里对端还留着 192.168.2.225
/// 和 .226。这两个地址包发出去石沉大海，只能走满 `DIAL_TIMEOUT`；而 Tailscale
/// 地址 15 毫秒就握手成功了，却被迫陪等 3 秒。
///
/// 这类地址不是"慢"，是**永远不会有结论**，等多久都一样。
#[test]
fn a_hopeless_high_priority_address_does_not_hold_the_connection_hostage() {
    // 两个"跨网段"地址排在前面（2 秒后才失败），第三个 50ms 就连上。
    let list = addrs(&[47684, 47684, 47683]);
    let started = Instant::now();
    let got = race_by_priority(&list, fake_connect);
    let took = started.elapsed();

    assert_eq!(got, Some((2, 47683)));
    assert!(
        // 同上：宽到能容忍慢机器，但仍远小于 2000ms 的"等满超时"。
        took < Duration::from_millis(1500),
        "应在宽限期后就走，而不是陪着高优先级等满超时；实际 {took:?}"
    );
    assert!(
        took >= Duration::from_millis(300),
        "宽限期内还是得给高优先级机会，不能一连上就跑；实际 {took:?}"
    );
}

/// 全部失败要如实返回 None，而不是卡住。
#[test]
fn all_failures_return_none() {
    assert!(race_by_priority(&addrs(&[47681, 47681, 47681]), fake_connect).is_none());
    assert!(race_by_priority(&[], fake_connect).is_none());
}
