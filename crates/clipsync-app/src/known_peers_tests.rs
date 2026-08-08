//! `known_peers` 的单元测试（单独成文件以控制行数，仍是其子模块）。

use super::*;

/// 台数句柄必须与表本身寸步不离——托盘的「已连接 m / n 台」里的 n 就是它。
///
/// 回归自实机反馈「已连接 2 / 1 台」：托盘原先自己存了一份台数，靠配对流程
/// 手工同步，而引荐认识的设备和对端广播的移出都不走配对流程。
#[test]
fn counter_tracks_every_mutation() {
    let known = KnownPeers::new(vec![peer("a", 1)]);
    let n = known.counter();
    assert_eq!(n.load(std::sync::atomic::Ordering::Acquire), 1);

    known.upsert(peer("b", 2));
    assert_eq!(
        n.load(std::sync::atomic::Ordering::Acquire),
        2,
        "引荐认识一台"
    );

    known.upsert(peer("b", 2)); // 重复登记不该把台数记成三台
    assert_eq!(n.load(std::sync::atomic::Ordering::Acquire), 2);

    known.remove(&peer("b", 2).device);
    assert_eq!(
        n.load(std::sync::atomic::Ordering::Acquire),
        1,
        "对端广播移出"
    );

    known.remove(&peer("ghost", 9).device); // 不存在的设备
    assert_eq!(n.load(std::sync::atomic::Ordering::Acquire), 1);

    assert_eq!(known.len(), 1, "len 与句柄同源");
}

fn peer(id: &str, key: u8) -> KnownPeer {
    KnownPeer {
        device: DeviceId::from_public_key(id.as_bytes()),
        name: id.to_string(),
        static_public_key: vec![key; 32],
    }
}

/// 运行期新增的设备必须立刻对拨号与入站认证可见。
///
/// 配对可以从托盘发起，此时进程正在运行。若这张表是启动快照，用户会
/// 看到"配对成功"却什么都同步不了，且没有任何提示说该重启。
#[test]
fn newly_paired_device_is_visible_immediately() {
    let known = KnownPeers::new(vec![peer("a", 1)]);
    let shared = known.clone(); // 拨号线程持有的那一份

    let b = peer("b", 2);
    known.upsert(b.clone());

    assert_eq!(shared.len(), 2, "克隆出的句柄应看到新设备");
    assert!(shared.contains(&b.device), "拨号线程据此决定拨谁");
    assert_eq!(
        shared
            .find_by_static_key(&b.static_public_key)
            .map(|p| p.name),
        Some("b".to_string()),
        "入站握手据静态公钥认证，查不到就会拒绝这台新配对的设备"
    );
}

/// 重复配对同一设备只更新、不产生第二条记录。
#[test]
fn upsert_replaces_instead_of_duplicating() {
    let known = KnownPeers::new(vec![peer("a", 1)]);

    let mut renamed = peer("a", 9);
    renamed.name = "改了名的 A".to_string();
    known.upsert(renamed);

    assert_eq!(known.len(), 1, "同一 device id 不应出现两条");
    let got = known.snapshot().pop().unwrap();
    assert_eq!(got.name, "改了名的 A");
    assert_eq!(
        got.static_public_key,
        vec![9; 32],
        "公钥应更新为最新一次配对的"
    );
}

/// 解除配对后，拨号与入站认证都必须立刻查不到这台设备。
#[test]
fn removed_device_is_invisible_to_dialer_and_auth() {
    let a = peer("a", 1);
    let b = peer("b", 2);
    let known = KnownPeers::new(vec![a.clone(), b.clone()]);
    let shared = known.clone();

    assert!(known.remove(&b.device), "应报告确实移除了");

    assert_eq!(shared.len(), 1);
    assert!(!shared.contains(&b.device), "拨号线程不应再拨已解除的设备");
    assert!(
        shared.find_by_static_key(&b.static_public_key).is_none(),
        "入站握手应查不到其公钥，从而拒绝连接"
    );
    // 没被解除的那台不受影响。
    assert!(shared.contains(&a.device));
}

#[test]
fn removing_absent_device_reports_false() {
    let known = KnownPeers::new(vec![peer("a", 1)]);
    let ghost = peer("ghost", 9);
    assert!(!known.remove(&ghost.device));
    assert_eq!(known.len(), 1);
}

#[test]
fn unknown_static_key_is_not_found() {
    let known = KnownPeers::new(vec![peer("a", 1)]);
    assert!(
        known.find_by_static_key(&[7u8; 32]).is_none(),
        "未配对的公钥必须查不到——这是拒绝陌生连接的依据"
    );
}

/// 引荐机制的核心不变量：学到新设备后，它对拨号与入站认证立即可见。
///
/// 场景：A 分别与 B、C 配对，B 和 C 互不认识。A 连上 B 时把 C 引荐过去，
/// B 学到 C 之后就该能直连它——**即便 A 随后关机**。
#[test]
fn introduced_device_becomes_connectable() {
    let b_side = KnownPeers::new(vec![peer("a", 1)]); // B 起初只认识 A
    let c = peer("c", 3);

    assert!(!b_side.contains(&c.device), "前置条件：B 还不认识 C");
    assert!(
        b_side.find_by_static_key(&c.static_public_key).is_none(),
        "前置条件：C 来连会被「对端未配对」拒绝"
    );

    // A 把 C 引荐给 B。
    b_side.upsert(c.clone());

    assert!(b_side.contains(&c.device), "拨号线程现在会拨 C");
    assert_eq!(
        b_side
            .find_by_static_key(&c.static_public_key)
            .map(|p| p.name),
        Some("c".to_string()),
        "C 主动连过来时也能通过认证——A 在不在线都不影响"
    );
}

/// 重复引荐不得产生第二条记录，也不得把已有配对顶掉。
#[test]
fn repeated_introduction_is_idempotent() {
    let known = KnownPeers::new(vec![peer("a", 1), peer("c", 3)]);
    known.upsert(peer("c", 3));
    known.upsert(peer("c", 3));
    assert_eq!(known.len(), 2, "同一设备被反复引荐仍只有一条");
}

/// 设备表一变，版本号就得变——各连接靠它知道"该重新引荐了"。
///
/// 没有它就只能定时轮询：A 与 B 连上时 B 还只认识 A，等 B 后来配了 C，
/// 那条已建立的连接不会再引荐，A 要等一整个周期才知道 C 的存在。用户的
/// 感受是"刚配完却没生效"。
#[test]
fn version_changes_on_every_mutation() {
    let known = KnownPeers::new(vec![peer("a", 1)]);
    let v0 = known.version();

    known.upsert(peer("b", 2));
    let v1 = known.version();
    assert_ne!(v1, v0, "新增设备应推进版本");

    // 覆盖已有设备（比如重新配对）同样是变化。
    known.upsert(peer("b", 9));
    let v2 = known.version();
    assert_ne!(v2, v1, "更新已有设备也应推进版本");

    assert!(known.remove(&peer("b", 9).device));
    assert_ne!(known.version(), v2, "移除设备应推进版本");
}

/// 没真的改动时不该推进版本，否则每轮都会白引荐一次。
#[test]
fn version_stays_put_when_nothing_removed() {
    let known = KnownPeers::new(vec![peer("a", 1)]);
    let v = known.version();
    assert!(!known.remove(&peer("ghost", 9).device));
    assert_eq!(known.version(), v, "移除不存在的设备不算变化");
}

/// 设备表一变，等在上面的线程必须立刻醒。
///
/// 回归自实机观感："配对完还得等半天，引荐更慢"。日志量出来是 45 秒——
/// 拨号线程死等一个退避间隔，而退避连不上时会翻倍到上限。
#[test]
fn waiting_thread_wakes_the_moment_a_device_is_added() {
    use std::time::{Duration, Instant};

    let known = KnownPeers::new(vec![]);
    let version = known.version();

    let waiter = known.clone();
    let t0 = Instant::now();
    let h = std::thread::spawn(move || {
        // 退避上限量级的等待；被唤醒才算通过。
        waiter.wait_for_change(version, Duration::from_secs(60));
        t0.elapsed()
    });

    std::thread::sleep(Duration::from_millis(50));
    known.upsert(peer("b", 2));

    let waited = h.join().unwrap();
    assert!(
        waited < Duration::from_secs(2),
        "登记新设备后应立刻醒，实际等了 {waited:?}"
    );
}

/// 已经变过了就不该再等——否则调用方在"变更发生在检查与等待之间"时会白等
/// 一整个超时。
#[test]
fn waiting_returns_at_once_when_the_change_already_happened() {
    use std::time::{Duration, Instant};

    let known = KnownPeers::new(vec![]);
    let stale = known.version();
    known.upsert(peer("b", 2));

    let t0 = Instant::now();
    known.wait_for_change(stale, Duration::from_secs(60));
    // 判的是"根本没等"，不是"等得短"。给到 500ms 纯粹为容忍 CI 的线程调度
    // 抖动——真退化成阻塞等待的话是秒级，照样区分得开。
    assert!(t0.elapsed() < Duration::from_millis(500), "不该等");
}
