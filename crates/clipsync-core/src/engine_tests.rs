//! `engine` 的单元测试。
//!
//! 单独成文件只为让 `engine.rs` 保持在项目约定的行数以内；
//! 它仍是 `engine` 的子模块（由 `#[path]` 引入），`use super::*`
//! 照常可用，与写在原文件里没有区别。

use super::*;
use crate::content::{ClipContent, ImageData};

fn engine() -> SyncEngine {
    SyncEngine::new(DeviceId::from_public_key(b"me"), Limits::default())
}

fn text(s: &str) -> ClipContent {
    ClipContent::Text(s.into())
}

#[test]
fn first_local_change_broadcasts_with_seq_zero() {
    let mut e = engine();
    match e.on_local_change(&text("hi"), false) {
        LocalDecision::Broadcast { seq, content_hash } => {
            assert_eq!(seq, 0);
            assert_eq!(content_hash, text("hi").content_hash());
        }
        other => panic!("expected broadcast, got {other:?}"),
    }
}

#[test]
fn seq_increments_across_distinct_changes() {
    let mut e = engine();
    let s0 = match e.on_local_change(&text("a"), false) {
        LocalDecision::Broadcast { seq, .. } => seq,
        _ => panic!(),
    };
    let s1 = match e.on_local_change(&text("b"), false) {
        LocalDecision::Broadcast { seq, .. } => seq,
        _ => panic!(),
    };
    assert_eq!((s0, s1), (0, 1));
}

#[test]
fn duplicate_local_change_is_skipped() {
    let mut e = engine();
    assert!(matches!(
        e.on_local_change(&text("dup"), false),
        LocalDecision::Broadcast { .. }
    ));
    assert_eq!(
        e.on_local_change(&text("dup"), false),
        LocalDecision::Skip(SkipReason::Duplicate)
    );
}

#[test]
fn sensitive_content_is_skipped() {
    let mut e = engine();
    assert_eq!(
        e.on_local_change(&text("secret"), true),
        LocalDecision::Skip(SkipReason::Sensitive)
    );
}

/// 回归测试：敏感标记消失后，同一内容仍不得外传。
///
/// 真实泄漏路径（Windows 上实测确认）：密码管理器写入"密码 + 排除标记"，
/// 进程退出时系统 flush 剪贴板丢弃了空数据的自定义格式，文本却仍在。
/// 剪贴板序列号随之变化，监听器重新读到的是"无标记的密码"——若只看当下
/// 标记就会把它广播出去。
#[test]
fn sensitive_content_stays_blocked_after_marker_disappears() {
    let mut e = engine();
    let secret = text("my-password-123");

    // 第一次：带标记，正确跳过。
    assert_eq!(
        e.on_local_change(&secret, true),
        LocalDecision::Skip(SkipReason::Sensitive)
    );

    // 第二次：标记已消失（sensitive=false），但内容相同——必须继续跳过。
    assert_eq!(
        e.on_local_change(&secret, false),
        LocalDecision::Skip(SkipReason::Sensitive),
        "标记消失后密码不得被广播"
    );

    // 其它内容不受影响，正常同步。
    assert!(matches!(
        e.on_local_change(&text("normal text"), false),
        LocalDecision::Broadcast { .. }
    ));
}

/// 敏感哈希记录有容量上限，不会无限增长。
#[test]
fn sensitive_memory_is_bounded() {
    let mut e = engine();
    for i in 0..(SENSITIVE_MEMORY + 10) {
        let _ = e.on_local_change(&text(&format!("secret-{i}")), true);
    }
    assert!(
        e.sensitive_hashes.len() <= SENSITIVE_MEMORY,
        "敏感哈希集合应受容量限制，实际 {}",
        e.sensitive_hashes.len()
    );
    // 最近的仍应被记住。
    let recent = text(&format!("secret-{}", SENSITIVE_MEMORY + 9));
    assert_eq!(
        e.on_local_change(&recent, false),
        LocalDecision::Skip(SkipReason::Sensitive)
    );
}

#[test]
fn oversized_content_is_skipped() {
    let mut e = SyncEngine::new(
        DeviceId::from_public_key(b"me"),
        Limits {
            max_bytes: 4,
            ..Default::default()
        },
    );
    assert_eq!(
        e.on_local_change(&text("toolong"), false),
        LocalDecision::Skip(SkipReason::TooLarge)
    );
}

#[test]
fn disabled_kind_is_skipped() {
    let mut e = SyncEngine::new(
        DeviceId::from_public_key(b"me"),
        Limits {
            allow_image: false,
            ..Default::default()
        },
    );
    let img = ClipContent::Image(ImageData {
        width: 1,
        height: 1,
        rgba: vec![0, 0, 0, 0],
    });
    assert_eq!(
        e.on_local_change(&img, false),
        LocalDecision::Skip(SkipReason::KindDisabled)
    );
}

#[test]
fn paused_engine_skips_both_directions() {
    let mut e = engine();
    e.set_paused(true);
    assert_eq!(
        e.on_local_change(&text("x"), false),
        LocalDecision::Skip(SkipReason::Paused)
    );
    let h = text("y").content_hash();
    assert_eq!(
        e.on_remote_clip(&text("y"), h),
        RemoteDecision::Skip(SkipReason::Paused)
    );
}

#[test]
fn remote_clip_applies_and_updates_state() {
    let mut e = engine();
    let c = text("from-peer");
    let h = c.content_hash();
    assert_eq!(e.on_remote_clip(&c, h), RemoteDecision::Apply);
    // 再次收到相同内容应去重。
    assert_eq!(
        e.on_remote_clip(&c, h),
        RemoteDecision::Skip(SkipReason::Duplicate)
    );
}

/// 关键防回环测试：远端内容写入本地后触发的本地变化不得被再次广播。
#[test]
fn applying_remote_then_local_echo_is_suppressed() {
    let mut e = engine();
    let c = text("round-trip");
    let h = c.content_hash();

    // 1) 收到远端内容，决定写入。
    assert_eq!(e.on_remote_clip(&c, h), RemoteDecision::Apply);
    // 2) 写入前登记预期回声。
    e.expect_echo(h);
    // 3) 平台监听触发本地变化（同内容）—— 必须被识别为回声并跳过。
    assert_eq!(
        e.on_local_change(&c, false),
        LocalDecision::Skip(SkipReason::Echo)
    );
    // 4) 回声已消费；此后用户真实复制不同内容仍应广播。
    assert!(matches!(
        e.on_local_change(&text("user-typed"), false),
        LocalDecision::Broadcast { .. }
    ));
}

#[test]
fn echo_registration_is_one_shot() {
    let mut e = engine();
    let c = text("once");
    let h = c.content_hash();
    e.expect_echo(h);
    // 第一次是回声。
    assert_eq!(
        e.on_local_change(&c, false),
        LocalDecision::Skip(SkipReason::Echo)
    );
    // 若用户之后又复制相同内容：因 last_hash 已是该值，走去重而非回声，
    // 同样不会误广播。
    assert_eq!(
        e.on_local_change(&c, false),
        LocalDecision::Skip(SkipReason::Duplicate)
    );
}

/// 未兑现的回声登记不得无限堆积。
///
/// 真实场景：远端内容写入本地剪贴板后，用户立刻复制了别的东西，去抖动
/// 把这次变化合并掉——那条登记就再也没人消费。常驻进程一跑几个月，
/// 若不限容，集合会随同步次数单调增长。
#[test]
fn pending_echo_is_bounded() {
    let mut e = engine();
    for i in 0..(PENDING_ECHO_CAPACITY * 4) {
        e.expect_echo(text(&format!("never-echoed-{i}")).content_hash());
    }
    assert!(
        e.pending_echo.len() <= PENDING_ECHO_CAPACITY,
        "回声登记应受容量限制，实际 {}",
        e.pending_echo.len()
    );
    assert_eq!(
        e.pending_echo.len(),
        e.pending_echo_order.len(),
        "集合与顺序表必须同步，否则淘汰会漏删"
    );

    // 最近登记的仍应被当作回声抑制——限容不能牺牲防回环这个本职。
    let recent = text(&format!("never-echoed-{}", PENDING_ECHO_CAPACITY * 4 - 1));
    assert_eq!(
        e.on_local_change(&recent, false),
        LocalDecision::Skip(SkipReason::Echo)
    );
}

/// 消费一条回声后，顺序表也要同步摘除，否则淘汰时会误删仍有效的登记。
#[test]
fn consuming_echo_keeps_order_list_in_sync() {
    let mut e = engine();
    let a = text("aaa");
    let b = text("bbb");
    e.expect_echo(a.content_hash());
    e.expect_echo(b.content_hash());

    assert_eq!(
        e.on_local_change(&a, false),
        LocalDecision::Skip(SkipReason::Echo)
    );
    assert_eq!(e.pending_echo.len(), 1);
    assert_eq!(e.pending_echo_order.len(), 1, "顺序表未同步摘除已消费的登记");

    // b 的登记仍在，仍应被抑制。
    assert_eq!(
        e.on_local_change(&b, false),
        LocalDecision::Skip(SkipReason::Echo)
    );
    assert!(e.pending_echo.is_empty());
    assert!(e.pending_echo_order.is_empty());
}

/// 文件传输失败后必须能重试：清除记录前会被判为重复，清除后应可重新接收。
#[test]
fn forget_current_allows_retry_after_failed_transfer() {
    let mut e = engine();
    let c = text("file-placeholder");
    let h = c.content_hash();

    assert_eq!(e.on_remote_clip(&c, h), RemoteDecision::Apply);
    // 传输中断——若不清除记录，对端重发会被当作重复而永远同步不过来。
    assert_eq!(
        e.on_remote_clip(&c, h),
        RemoteDecision::Skip(SkipReason::Duplicate)
    );

    e.forget_current();
    assert_eq!(
        e.on_remote_clip(&c, h),
        RemoteDecision::Apply,
        "清除记录后应允许重新接收同一内容"
    );
}
