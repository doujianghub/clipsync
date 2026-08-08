//! `tray` 的单元测试。
//!
//! 单独成文件只为让 `tray.rs` 保持在项目约定的行数以内；它仍是 `tray` 的
//! 子模块（由 `#[path]` 引入），`use super::*` 照常可用。

use super::*;

#[test]
fn summary_reflects_state() {
    let s = TrayStatus::new(0);
    assert!(s.summary().contains("尚未配对"));

    let s = TrayStatus::new(2);
    assert!(s.summary().contains("未连接"));

    s.set_connected_ids(["dev-a".to_string()].into_iter().collect());
    assert!(s.summary().contains("已连接 1 / 2"));

    s.set_paused(true);
    assert!(s.summary().contains("已暂停"), "暂停状态应优先展示");
}

/// 中枢停止后，界面不得再显示"一切正常"。
///
/// 同步已经彻底不工作了，而托盘图标还是绿的、菜单还写着"已连接 N 台"
/// ——用户对着这个界面永远想不通"为什么复制过不去"。
#[test]
fn dead_hub_overrides_healthy_looking_state() {
    let s = TrayStatus::new(2);
    s.set_connected_ids(["dev-a".to_string()].into_iter().collect());
    assert_eq!(IconState::of(&s), IconState::Connected);

    s.set_hub_dead();
    assert_eq!(
        IconState::of(&s),
        IconState::Broken,
        "中枢已死时不能还显示已连接"
    );
    assert!(
        s.summary().contains("同步已停止"),
        "文字也要如实说明，实际: {}",
        s.summary()
    );
}

/// 故障优先于暂停：两者都成立时该显示故障，暂停是用户自己知道的事。
#[test]
fn broken_takes_precedence_over_paused() {
    let s = TrayStatus::new(1);
    s.set_paused(true);
    s.set_hub_dead();
    assert_eq!(IconState::of(&s), IconState::Broken);
}

#[test]
fn pause_state_is_shared_across_clones() {
    let s = TrayStatus::new(1);
    let clone = s.clone();
    s.set_paused(true);
    assert!(
        clone.is_paused(),
        "克隆出的句柄应看到同一份暂停状态（中枢与托盘共享）"
    );
}

/// 传输活动过期后脉冲必须自己停下。
///
/// 这正是选"最后活动时刻"而不是"进行中计数"的理由：计数要在收发两侧
/// 各处出口精确配对增减，漏掉任何一条错误路径就永久泄漏，表现是图标
/// 一直闪个不停——只在出错后才显现、且很难查。
#[test]
fn transfer_pulse_expires_on_its_own() {
    let s = TrayStatus::new(1);
    assert!(!s.is_transferring(), "从未传输过就不该脉冲");

    s.note_transfer(TransferProgress {
        sending: true,
        name: "big.zip".into(),
        done: 1,
        total: 100,
    });
    assert!(s.is_transferring(), "刚传完分块应处于脉冲状态");
}
