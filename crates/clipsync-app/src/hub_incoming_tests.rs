//! `hub_incoming` 的单元测试。
//!
//! 单独成文件只为让 `hub_incoming.rs` 保持在项目约定的行数以内。

use super::should_auto_fetch;

/// 上限以内自动拉，超了就挂起——这是「自动取回上限」的全部含义。
#[test]
fn size_decides_whether_to_pull_now() {
    const LIMIT: u64 = 100 << 20; // 100 MiB
    assert!(should_auto_fetch(1, LIMIT, false));
    assert!(should_auto_fetch(LIMIT, LIMIT, false), "正好到线仍算以内");
    assert!(
        !should_auto_fetch(LIMIT + 1, LIMIT, false),
        "过线一个字节就挂起"
    );
    assert!(!should_auto_fetch(5 << 30, LIMIT, false));
}

/// 缓存里整份都有就直接拉，多大都一样——那是零传输。
///
/// 来回切换同一个大文件、断线重连后重复通告都会走这条路。这时还让人去
/// 点一下「取回」纯属多余，而且那个词也名不副实：根本没什么要取。
#[test]
fn a_fully_cached_batch_never_waits_for_a_click() {
    assert!(should_auto_fetch(500 << 30, 1, true), "缓存命中就不该挂起");
}

/// 「不限」意味着多大都自动拉。
#[test]
fn unlimited_pulls_everything() {
    assert!(should_auto_fetch(u64::MAX, usize::MAX as u64, false));
}
