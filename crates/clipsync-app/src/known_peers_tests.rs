//! `known_peers` 的单元测试（单独成文件以控制行数，仍是其子模块）。

use super::*;

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
        shared.find_by_static_key(&b.static_public_key).map(|p| p.name),
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
    assert_eq!(got.static_public_key, vec![9; 32], "公钥应更新为最新一次配对的");
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
        b_side.find_by_static_key(&c.static_public_key).map(|p| p.name),
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
