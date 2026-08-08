//! `net_manager` 的单元测试。
//!
//! 单独成文件只为让 `net_manager.rs` 保持在项目约定的行数以内。

use super::*;

/// 一轮拨号的墙上时间必须是**一个**超时窗口，而不是"候选数 × 超时"。
///
/// 回归自实机：启动后两台在线设备分别等了 10 秒和 29 秒才连上。地址簿里
/// 一台有 5 个候选、一台有 3 个，串行逐个 `connect_timeout(3s)` 试下来，
/// 光等超时就要 15 秒和 9 秒——而拨号线程还要挨个设备走一遍。
///
/// 退避上限直接决定"对端重新上线后最坏等多久"。
///
/// 方向去重规定只由 id 较小的一方拨号，所以较大的那一方完全被动——它启动
/// 后能多快连上，等的就是对方这个退避周期。实机上两台在线设备要干等
/// 10 秒和 29 秒，就是这么来的。
///
/// 之所以敢把上限压到 15 秒，是因为一轮拨号的代价变了：候选地址从串行
/// `connect_timeout` 改成并发，一轮的墙上时间从"候选数 × 3 秒"降到
/// "一个 3 秒窗口"。这条断言把两者绑在一起——将来谁把并发改回串行，
/// 就该重新掂量这个上限。
#[test]
fn backoff_cap_is_short_enough_to_not_be_noticed() {
    assert!(
        DIAL_RETRY_MAX <= Duration::from_secs(20),
        "上限就是被动方的最坏等待时间，太长会让人以为程序没在工作"
    );
    assert!(
        DIAL_RETRY_MAX >= DIAL_TIMEOUT * 2,
        "也不能短到上一轮的超时还没走完就又发起一轮"
    );
}

/// 入站连接也算"网络在好转"，不该继续退避。
///
/// 盲点回归：原先只看"本轮我拨通了谁"。对端主动连进来时网络明明是好的，
/// 我们自己的退避却照涨，另一台设备就得多等好几轮。
#[test]
fn an_inbound_connection_also_resets_the_backoff() {
    let mut d = DIAL_RETRY_INTERVAL;
    for _ in 0..8 {
        d = next_backoff(d, false, false, false);
    }
    assert!(d > DIAL_RETRY_INTERVAL, "前置条件：已退避到较大间隔");
    // 调用方把"自己拨通"与"对端连进来"合并成同一个入参，这里验的是
    // 后者同样能复位。
    assert_eq!(next_backoff(d, true, false, false), DIAL_RETRY_INTERVAL);
}

/// 连不上时逐轮加倍并停在上限——不能无限增长，否则恢复联网后要等太久。
#[test]
fn backoff_grows_then_caps() {
    let mut d = DIAL_RETRY_INTERVAL;
    let mut seen = vec![d];
    for _ in 0..10 {
        d = next_backoff(d, false, false, false);
        seen.push(d);
    }
    assert!(
        seen.windows(2).all(|w| w[1] >= w[0]),
        "间隔应单调不减，实际 {seen:?}"
    );
    assert_eq!(d, DIAL_RETRY_MAX, "应停在上限而不是无限增长");
}

/// 连上过就必须复位。忘了这一步的表现是"断网一次之后就再也不积极重连"。
#[test]
fn backoff_resets_after_success() {
    let mut d = DIAL_RETRY_INTERVAL;
    for _ in 0..8 {
        d = next_backoff(d, false, false, false);
    }
    assert!(d > DIAL_RETRY_INTERVAL, "前置条件：已退避到较大间隔");

    assert_eq!(
        next_backoff(d, true, false, false),
        DIAL_RETRY_INTERVAL,
        "连上后应立刻回到起始间隔"
    );
}

/// 刚认识、地址还没到的设备不该拖长退避。
///
/// 回归自实机：`经 KPC 认识了 MacBook Pro` 到真正连上隔了 **60.019 秒**，
/// 正好是 `DIAL_RETRY_MAX`。根因是引荐登记时先动设备表、后写地址簿，
/// 而设备表一变就唤醒拨号线程——它醒来看到一台没有任何地址的设备，
/// 拨不出去，就把这一轮记成"连不上"，退避翻倍直到上限。
///
/// 顺序已经改对（地址先落地址簿），这条断言是第二道防线：就算将来又有谁
/// 把顺序写反，最坏也只是慢一个起始间隔，而不是慢一分钟。
#[test]
fn backoff_does_not_grow_while_waiting_for_addresses() {
    let mut d = DIAL_RETRY_INTERVAL;
    for _ in 0..8 {
        d = next_backoff(d, false, false, false);
    }
    assert!(d > DIAL_RETRY_INTERVAL, "前置条件：已退避到较大间隔");

    assert_eq!(
        next_backoff(d, false, true, false),
        DIAL_RETRY_INTERVAL,
        "地址还没到不是连不上，不该继续拉长间隔"
    );
}

/// 设备表变过就复位：新设备是新的机会，不该继承此前攒下的长间隔。
#[test]
fn backoff_resets_when_the_device_table_changes() {
    let mut d = DIAL_RETRY_INTERVAL;
    for _ in 0..8 {
        d = next_backoff(d, false, false, false);
    }
    assert_eq!(next_backoff(d, false, false, true), DIAL_RETRY_INTERVAL);
}

/// 整套「两边都拨」方案的地基：**同一条 socket，两端必然算出同一个结论**。
///
/// 两端并不通信，各自只知道两个 device id 和"我是拨出还是接入"。若这两个
/// 信息推不出一致的结论，双方就可能留下不同的那条连接——而那两条 socket
/// 是同一对连接的两端，各留一条的结果是**两条全死**。
///
/// 顺带验第二条同样要命的性质：两条候选里**恰好一条**是规范的。若一条都不
/// 规范，两边会互相谦让到谁都连不上；若两条都规范，去重就形同虚设。
#[test]
fn both_ends_of_a_socket_agree_on_which_connection_wins() {
    // 取真实形态的 id（十六进制串），并刻意包含前缀相同、长度相同、
    // 以及字典序相邻的组合。
    let ids = [
        "86616331d59758e7",
        "d299082d41a4645a",
        "d9041402e76da45d",
        "0000000000000001",
        "ffffffffffffffff",
        "d299082d41a4645b", // 与上面只差最后一位
    ];
    for a in ids {
        for b in ids {
            if a == b {
                continue; // 同一台设备不会连自己
            }
            let (a, b) = (DeviceId::from_hex(a), DeviceId::from_hex(b));

            // socket ①：由 a 拨向 b。a 看是出站，b 看是入站。
            let at_dialer = is_canonical(&a, &b, true);
            let at_listener = is_canonical(&b, &a, false);
            assert_eq!(
                at_dialer, at_listener,
                "a={a} b={b}：a 拨出的那条，两端结论必须一致"
            );

            // socket ②：由 b 拨向 a。
            let other = is_canonical(&b, &a, true);
            assert_eq!(other, is_canonical(&a, &b, false), "b 拨出的那条同理");

            // 两条里恰好一条规范。
            assert_ne!(
                at_dialer, other,
                "a={a} b={b}：必须恰好一条是规范方向，不能都是或都不是"
            );
        }
    }
}

/// 规范的那条就是「小 id 拨出」的那条——退避与日志里的措辞都按这个理解。
#[test]
fn the_canonical_connection_is_the_one_dialed_by_the_smaller_id() {
    let small = DeviceId::from_hex("86616331d59758e7");
    let big = DeviceId::from_hex("d9041402e76da45d");

    assert!(is_canonical(&small, &big, true), "小 id 拨出：规范");
    assert!(is_canonical(&big, &small, false), "大 id 接入：同一条，也规范");
    assert!(!is_canonical(&big, &small, true), "大 id 拨出：兜底，让位");
    assert!(!is_canonical(&small, &big, false), "小 id 接入：同一条，也让位");
}

/// 让位窗口要远大于常见 RTT，否则"规范连接恰好卡在截止点"的区间就不够窄。
#[test]
fn the_yield_window_leaves_room_for_a_round_trip() {
    assert!(
        NONCANONICAL_YIELD >= Duration::from_millis(300),
        "窗口要盖得住一个 RTT 的两端完成时间差，否则两端可能选中不同的连接"
    );
    assert!(
        NONCANONICAL_YIELD < DIAL_RETRY_INTERVAL,
        "又不能长到让兜底连接拖过下一轮拨号"
    );
}
