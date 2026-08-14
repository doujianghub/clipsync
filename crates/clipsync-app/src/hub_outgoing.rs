//! 中枢的**发送侧决策**：本地剪贴板变了，要不要广播、广播给谁、不发的话
//! 怎么让用户知道。
//!
//! 与 `hub_incoming` 对称——那边是"收到的怎么落地"，这边是"自己的怎么发出
//! 去"。两侧的判断依据完全不同：发送侧看的是引擎的过滤规则（敏感内容、
//! 类型开关、内联硬上限），接收侧看的是本机的自动取回上限与缓存命中。

use std::sync::Arc;

use clipsync_core::{ClipContent, LocalDecision, SkipReason, SyncMessage};
use clipsync_net::prepared::PreparedMessage;
use tracing::{debug, info};

use super::HubState;

impl HubState {
    /// 处理本地剪贴板变化：判定后广播给所有对端。
    pub(super) fn on_local(
        &mut self,
        content: ClipContent,
        sensitive: bool,
        file_paths: Vec<std::path::PathBuf>,
    ) {
        match self.engine.on_local_change(&content, sensitive) {
            LocalDecision::Broadcast { seq, content_hash } => {
                let kind = kind_label(&content);
                let size = content.byte_size();

                // 文件内容不随消息发送，只登记路径等待对端索取。
                // 推进代际号会立即中止任何仍在进行的旧文件传输。
                match &content {
                    ClipContent::Files(metas) => {
                        let pairs: Vec<(u64, std::path::PathBuf)> = metas
                            .iter()
                            .zip(file_paths.iter())
                            .map(|(m, p)| (m.id, p.clone()))
                            .collect();
                        self.deps.outgoing.register(seq, pairs);
                    }
                    _ => self.deps.outgoing.advance(seq),
                }

                let msg = SyncMessage::Clip {
                    origin: self.engine.device_id().clone(),
                    seq,
                    content_hash,
                    content,
                };
                if self.peers.is_empty() {
                    info!("已复制 [{kind}] {size} 字节，但暂无对端连接（seq={seq}）");
                    return;
                }

                // 编码压缩**只做一次**，N 台设备共享同一份字节。从前是每条连接
                // 各编各压：两台设备时日志里会出现两行一模一样的「帧层压缩」，
                // 一张 4K 截图白花 15 ms 序列化加 40 ms deflate 再加一份 33 MB
                // 内存副本，而且随设备数线性增长。
                let t0 = std::time::Instant::now();
                let Some(prepared) = self.prepare(&msg) else {
                    return;
                };
                crate::logging::note_slow("编码与压缩", t0);

                let wire = prepared.wire_len();
                for peer in self.peers.values() {
                    let _ = peer.tx.send(Arc::clone(&prepared));
                }
                info!(
                    "已同步 [{kind}] {size} 字节（上线 {wire}）→ {} 台设备",
                    self.peers.len()
                );
            }
            LocalDecision::Skip(reason) => {
                self.log_skip(reason, &content);
            }
        }
    }

    /// 编码一条待发消息，供广播与单播共用。
    ///
    /// 失败只可能是序列化出错（内容本身有问题），此时该条消息发不出去，但
    /// 中枢必须继续跑——所以返回 `None` 由调用方决定怎么说，而不是让整个
    /// 中枢线程崩掉。
    pub(super) fn prepare(&self, msg: &SyncMessage) -> Option<Arc<PreparedMessage>> {
        match PreparedMessage::encode(msg) {
            Ok(p) => Some(Arc::new(p)),
            Err(e) => {
                tracing::warn!("编码待发消息失败，本条不发送: {e:#}");
                None
            }
        }
    }

    /// 记录一次跳过。
    ///
    /// `TooLarge` 与 `KindDisabled` 是**用户需要知道**的——内容没同步过去，
    /// 而原因是可调的设置，若只写在 DEBUG 里，用户只会觉得"东西怎么没过去"
    /// 却无从查起（我们自己联调时就在 TooLarge 上栽过一次）。
    ///
    /// 但它们也可能连续触发（关掉图片同步后每次截图都会命中），所以同一原因
    /// 只在**第一次**打 INFO，重复时降到 DEBUG，原因变化后重新计数。
    /// `Echo`/`Duplicate` 每次复制都会出现，始终留在 DEBUG。
    fn log_skip(&mut self, reason: SkipReason, content: &ClipContent) {
        let noteworthy = matches!(reason, SkipReason::TooLarge | SkipReason::KindDisabled);
        let repeated = self.last_skip == Some(reason);
        self.last_skip = Some(reason);

        if !noteworthy || repeated {
            debug!("本地变化跳过 ({reason:?})");
            return;
        }

        match reason {
            SkipReason::TooLarge => info!(
                "{} 有 {} 字节，超过内联内容的硬上限 {} 字节，未同步",
                kind_label(content),
                content.byte_size(),
                clipsync_core::INLINE_MAX_BYTES
            ),
            SkipReason::KindDisabled => info!(
                "[{}] 未同步：该类型的发送已在托盘菜单中关闭",
                kind_label(content)
            ),
            _ => unreachable!("noteworthy 已限定分支"),
        }
    }
}

pub(super) fn kind_label(content: &ClipContent) -> &'static str {
    match content.kind() {
        clipsync_core::ContentKind::Text => "文本",
        clipsync_core::ContentKind::Image => "图片",
        clipsync_core::ContentKind::Files => "文件",
    }
}
