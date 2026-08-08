//! `pairing_session` 的单元测试。

use super::*;

/// 重复发起主持配对时，不得去 bind 一个已被占用的端口。
///
/// 用户第二次点「显示配对码」通常只是想再看一眼码。原先每次点击都新起
/// 线程去 bind 47685，第二次必然失败并把 `os error 10048` 抛给用户看，
/// 且只能重启程序恢复。槽位被占用时应能取出**当前会话的码**供重新显示。
#[test]
fn host_slot_reports_existing_code_instead_of_starting_over() {
    let slot = PairingHostSlot::default();

    let guard = slot.try_acquire().expect("首次应能占用");
    slot.begin("1234", Instant::now() + Duration::from_secs(180));

    assert!(
        slot.try_acquire().is_none(),
        "已有会话时不应再次占用槽位——那会撞上端口占用"
    );
    let live = slot.live().expect("应能取出当前会话的码以便重新显示");
    assert_eq!(live.code, "1234");
    assert!(live.remaining > Duration::from_secs(170));

    // 会话结束后必须能重新发起，否则功能就永久坏掉了。
    drop(guard);
    assert!(
        slot.try_acquire().is_some(),
        "会话结束后槽位应释放，允许发起新一轮配对"
    );
}

/// 槽位在 `host()` 报错返回时也要释放（靠 guard 的 Drop，不能靠成功路径）。
#[test]
fn host_slot_releases_on_error_path() {
    let slot = PairingHostSlot::default();
    {
        let _guard = slot.try_acquire().expect("应能占用");
        slot.begin("7890", Instant::now() + Duration::from_secs(180));
        // 模拟 host() 中途 bail!——guard 在作用域结束时析构。
    }
    assert!(
        slot.try_acquire().is_some(),
        "出错路径也必须释放槽位，否则一次配对失败就再也发起不了"
    );
    assert!(slot.live().is_none(), "会话结束后不该还报告有效的配对码");
}

/// 过期的码不算有效——菜单据此换回「显示配对码…」。
///
/// `host()` 察觉超时要等到下一个轮询周期（200ms），期间槽位还占着。
/// 若不在这里判过期，菜单会显示「剩 0:00」甚至更怪的数。
#[test]
fn an_expired_code_is_not_live() {
    let slot = PairingHostSlot::default();
    let _guard = slot.try_acquire().unwrap();
    slot.begin("1234", Instant::now() - Duration::from_secs(1));
    assert!(slot.live().is_none(), "已过期就不该再报告出来");
}

/// 码还没生成时不算有效——占位期只有几微秒，但菜单不能显示一个空码。
#[test]
fn a_placeholder_session_has_no_code_yet() {
    let slot = PairingHostSlot::default();
    let _guard = slot.try_acquire().unwrap();
    assert!(slot.live().is_none(), "占位期不该报告出一个空配对码");
    assert!(
        !slot.request_new_code(None),
        "还没有码可换，不该置起取消标志"
    );
}

/// 「换个配对码」置起取消标志，`host()` 据此收摊。
#[test]
fn requesting_a_new_code_cancels_the_round() {
    let slot = PairingHostSlot::default();
    let guard = slot.try_acquire().unwrap();
    slot.begin("1234", Instant::now() + Duration::from_secs(180));

    assert!(!guard.cancel.load(std::sync::atomic::Ordering::Acquire));
    assert!(slot.request_new_code(None));
    assert!(
        guard.cancel.load(std::sync::atomic::Ordering::Acquire),
        "host() 靠这个标志退出 accept 循环"
    );
}

/// 旧窗口上的「换个配对码」不得掐掉**新**一轮。
///
/// 弹窗关不掉也不会自己消失：用户可能让上一轮的窗口一直停在屏幕上，
/// 等这一轮开始了才想起去点它。按码核对，认的是"我那一轮"。
#[test]
fn a_stale_dialog_cannot_cancel_the_current_round() {
    let slot = PairingHostSlot::default();
    let guard = slot.try_acquire().unwrap();
    slot.begin("5678", Instant::now() + Duration::from_secs(180));

    assert!(
        !slot.request_new_code(Some("1234")),
        "上一轮的码对不上，不该动手"
    );
    assert!(!guard.cancel.load(std::sync::atomic::Ordering::Acquire));

    // 认得出自己那一轮时照常生效。
    assert!(slot.request_new_code(Some("5678")));
    assert!(guard.cancel.load(std::sync::atomic::Ordering::Acquire));
}
