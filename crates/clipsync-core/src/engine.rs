//! 同步引擎：平台无关的决策核心。
//!
//! 引擎不做任何 I/O，只回答两个问题：
//!   1. 本地剪贴板变化了 —— 要不要广播出去？（`on_local_change`）
//!   2. 收到远端剪贴板消息 —— 要不要写入本地系统剪贴板？（`on_remote_message`）
//!
//! 两个方向共用一套"防回环"状态：当我们把远端内容写入本地系统剪贴板时，
//! 平台监听会再次触发 `on_local_change`；引擎通过登记"预期写入哈希"识别
//! 这类回声并抑制，从而不会把刚收到的内容又广播回去（形成风暴）。

use crate::content::ClipContent;
use crate::device::DeviceId;

/// 大小/类型限制。
#[derive(Debug, Clone)]
pub struct Limits {
    /// 单次内容最大字节数（文本/图片/文件都适用）。默认 100 MiB。
    pub max_bytes: usize,
    /// 是否同步图片。
    pub allow_image: bool,
    /// 是否同步文件。
    pub allow_files: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 100 * 1024 * 1024,
            allow_image: true,
            allow_files: true,
        }
    }
}

/// 本地变化被跳过的原因（用于日志/托盘提示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 这是我们自己刚写入的内容触发的回声。
    Echo,
    /// 与上次广播内容相同（去重）。
    Duplicate,
    /// 超过大小上限。
    TooLarge,
    /// 该类型被禁用。
    KindDisabled,
    /// 平台层判定为敏感/瞬态内容。
    Sensitive,
    /// 同步已暂停。
    Paused,
}

/// 对"本地剪贴板变化"的决策结果。
#[derive(Debug, Clone, PartialEq)]
pub enum LocalDecision {
    /// 应广播：携带序号与内容哈希。
    Broadcast { seq: u64, content_hash: u64 },
    /// 跳过，附原因。
    Skip(SkipReason),
}

/// 对"收到远端消息"的决策结果。
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteDecision {
    /// 应写入本地系统剪贴板。
    Apply,
    /// 跳过（重复/暂停）。
    Skip(SkipReason),
}

/// 同步引擎状态机。
///
/// 单线程使用；`clipsync-app` 通过 channel 串行化本地事件与远端事件后
/// 交给引擎，避免并发。
#[derive(Debug)]
pub struct SyncEngine {
    device_id: DeviceId,
    limits: Limits,
    paused: bool,

    /// 本设备下一个要用的广播序号。
    next_seq: u64,

    /// 最近一次"我方状态"（无论来自本地复制还是应用远端内容）的内容哈希。
    /// 用于去重：本地再次出现相同内容不重复广播。
    last_hash: Option<u64>,

    /// 预期写入哈希：调用方即将/已把此内容写入系统剪贴板，
    /// 由此产生的下一次本地变化应被当作回声抑制。
    ///
    /// 用集合而非单值，容忍平台监听的轻微乱序/延迟。
    pending_echo: std::collections::HashSet<u64>,
}

impl SyncEngine {
    pub fn new(device_id: DeviceId, limits: Limits) -> Self {
        Self {
            device_id,
            limits,
            paused: false,
            next_seq: 0,
            last_hash: None,
            pending_echo: std::collections::HashSet::new(),
        }
    }

    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// 处理本地剪贴板变化。
    ///
    /// `sensitive` 由平台层判定（macOS ConcealedType / Windows 排除标记等）。
    /// 返回是否应广播；若广播，调用方用返回的 seq/hash 构造 `SyncMessage::Clip`。
    pub fn on_local_change(&mut self, content: &ClipContent, sensitive: bool) -> LocalDecision {
        if self.paused {
            return LocalDecision::Skip(SkipReason::Paused);
        }

        let hash = content.content_hash();

        // 1) 回声抑制：这是我们刚写入系统剪贴板的内容。
        if self.pending_echo.remove(&hash) {
            // 同步 last_hash，使后续同内容的真实本地复制也能正确去重。
            self.last_hash = Some(hash);
            return LocalDecision::Skip(SkipReason::Echo);
        }

        // 2) 敏感内容不外传。
        if sensitive {
            return LocalDecision::Skip(SkipReason::Sensitive);
        }

        // 3) 类型开关。
        match content.kind() {
            crate::content::ContentKind::Image if !self.limits.allow_image => {
                return LocalDecision::Skip(SkipReason::KindDisabled);
            }
            crate::content::ContentKind::Files if !self.limits.allow_files => {
                return LocalDecision::Skip(SkipReason::KindDisabled);
            }
            _ => {}
        }

        // 4) 大小上限。
        if content.byte_size() > self.limits.max_bytes {
            return LocalDecision::Skip(SkipReason::TooLarge);
        }

        // 5) 去重：与当前状态相同则不重复广播。
        if self.last_hash == Some(hash) {
            return LocalDecision::Skip(SkipReason::Duplicate);
        }

        // 通过：更新状态并分配序号。
        self.last_hash = Some(hash);
        let seq = self.next_seq;
        self.next_seq += 1;
        LocalDecision::Broadcast {
            seq,
            content_hash: hash,
        }
    }

    /// 处理来自远端的剪贴板消息。
    ///
    /// 返回是否应写入本地系统剪贴板。若应写入，调用方必须在**实际写入前**
    /// 调用 [`Self::expect_echo`] 登记该内容哈希，以抑制随之而来的本地回声。
    pub fn on_remote_clip(&mut self, content: &ClipContent, content_hash: u64) -> RemoteDecision {
        if self.paused {
            return RemoteDecision::Skip(SkipReason::Paused);
        }

        // 已经是当前内容 —— 无需重复写入（例如两端几乎同时复制了相同内容）。
        if self.last_hash == Some(content_hash) {
            return RemoteDecision::Skip(SkipReason::Duplicate);
        }

        // 记录为当前状态；防回环由 expect_echo 负责。
        self.last_hash = Some(content_hash);
        let _ = content; // 内容本身由调用方写入系统剪贴板
        RemoteDecision::Apply
    }

    /// 登记"即将写入系统剪贴板"的内容哈希，使随之触发的本地变化被识别为回声。
    ///
    /// 必须在真正写入系统剪贴板之前调用。
    pub fn expect_echo(&mut self, content_hash: u64) {
        self.pending_echo.insert(content_hash);
    }

    /// 清空待抑制回声（例如长时间未等到回声，避免集合无限增长）。
    pub fn clear_pending_echo(&mut self) {
        self.pending_echo.clear();
    }

    /// 忘记"当前内容"记录。
    ///
    /// 用于远端内容未能真正落地的情形——典型是文件内容传输中断或校验失败。
    /// 此时若仍记着该哈希，对端重新发送相同内容会被误判为重复而跳过，导致
    /// 用户永远同步不过来。清除后即可正常重试。
    pub fn forget_current(&mut self) {
        self.last_hash = None;
    }
}

#[cfg(test)]
mod tests {
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
}