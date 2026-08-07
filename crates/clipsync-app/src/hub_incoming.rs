//! 中枢的**接收侧文件状态机**。
//!
//! 从 `hub` 分出来是因为这部分自成一个闭环：查缓存 → 索取缺失分块 → 落盘 →
//! 校验 → 落地 → 写剪贴板，中间还要处理取代、断连、解压失败等中止路径。
//! 与"剪贴板事件分发"混在一个文件里会让两者都难读。
//!
//! **取代语义**：同一时刻只保留一个"接收中"的传输。收到更新的剪贴板内容时，
//! 旧传输立即作废——对端剪贴板里已经不是那份内容了，继续等只会让两端不一致。
//! 但**已收到的字节保留在缓存里**，将来再复制同一文件即可断点续传。

use clipsync_core::{DeviceId, FileMeta, SyncMessage};
use tracing::{debug, info, warn};

use super::HubState;

/// 一次进行中的文件接收。
pub(super) struct IncomingTransfer {
    pub(super) generation: u64,
    pub(super) from: DeviceId,
    pub(super) files: Vec<FileMeta>,
    /// 与 `files` 等长：各文件内容是否已完整落入缓存。
    pub(super) done: Vec<bool>,
}

impl IncomingTransfer {
    pub(super) fn all_done(&self) -> bool {
        self.done.iter().all(|d| *d)
    }

    pub(super) fn index_of(&self, file_id: u64) -> Option<usize> {
        self.files.iter().position(|f| f.id == file_id)
    }

    pub(super) fn pinned_ids(&self) -> Vec<u64> {
        self.files.iter().map(|f| f.id).collect()
    }
}

impl HubState {
    /// 开始接收一批文件：先查缓存，只索取缺失的部分。
    pub(super) fn begin_incoming_files(&mut self, from: DeviceId, generation: u64, files: Vec<FileMeta>) {
        // 新内容取代任何进行中的接收（其字节已在缓存中，将来可续传）。
        if self.incoming.is_some() {
            debug!("新的剪贴板内容取代了进行中的文件接收");
        }

        let mut done = Vec::with_capacity(files.len());
        let mut needs = Vec::new();
        let mut cached_bytes = 0u64;

        for f in &files {
            if self.deps.cache.is_complete(f.id, f.size) {
                done.push(true);
                cached_bytes += f.size;
            } else {
                done.push(false);
                let have = self.deps.cache.have_bytes(f.id, f.size);
                cached_bytes += have;
                needs.push((f.id, have));
            }
        }

        let total: u64 = files.iter().map(|f| f.size).sum();
        let transfer = IncomingTransfer {
            generation,
            from: from.clone(),
            files,
            done,
        };

        if transfer.all_done() {
            // 全部命中缓存：无需任何传输，立即落地。
            info!("文件已在缓存中命中，秒同步（{} 字节，零传输）", total);
            self.incoming = Some(transfer);
            self.finish_incoming();
            return;
        }

        if cached_bytes > 0 {
            info!(
                "开始接收 {} 个文件（共 {} 字节，已有 {} 字节，断点续传）",
                transfer.files.len(),
                total,
                cached_bytes
            );
        } else {
            info!("开始接收 {} 个文件（共 {} 字节）", transfer.files.len(), total);
        }

        self.incoming = Some(transfer);

        // 向对端索取缺失部分。
        for (file_id, offset) in needs {
            self.send_to(
                &from,
                SyncMessage::FileNeed {
                    generation,
                    file_id,
                    offset,
                },
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn on_file_chunk(
        &mut self,
        from: &DeviceId,
        generation: u64,
        file_id: u64,
        offset: u64,
        data: &[u8],
        compressed: bool,
        plain_len: u32,
    ) {
        // 过期代际的分块直接丢弃——剪贴板已经是别的内容了。
        if !self.matches_incoming(generation) {
            return;
        }

        // 压缩块先还原；`offset` 指的是原始文件位置，与是否压缩无关。
        let plain: std::borrow::Cow<[u8]> = if compressed {
            match crate::compress::decompress(data, plain_len as usize) {
                Ok(d) => std::borrow::Cow::Owned(d),
                Err(e) => {
                    warn!("解压文件分块失败，放弃本次传输: {e:#}");
                    self.incoming = None;
                    self.engine.forget_current();
                    return;
                }
            }
        } else {
            std::borrow::Cow::Borrowed(data)
        };

        if let Err(e) = self.deps.cache.append(file_id, offset, &plain) {
            // 偏移不连续通常意味着与另一次传输交错，放弃本次并让对端重来。
            warn!("写入文件分块失败: {e:#}");
            self.incoming = None;
            self.engine.forget_current();
            let _ = from;
        }
    }

    pub(super) fn on_file_done(
        &mut self,
        _from: &DeviceId,
        generation: u64,
        file_id: u64,
        content_hash: u64,
    ) {
        if !self.matches_incoming(generation) {
            return;
        }
        match self.deps.cache.finalize(file_id, content_hash) {
            Ok(()) => {
                if let Some(t) = self.incoming.as_mut() {
                    if let Some(i) = t.index_of(file_id) {
                        t.done[i] = true;
                    }
                }
                if self
                    .incoming
                    .as_ref()
                    .map(|t| t.all_done())
                    .unwrap_or(false)
                {
                    self.finish_incoming();
                }
            }
            Err(e) => {
                // 校验失败：宁可不同步，也绝不产生损坏文件。
                warn!("文件内容校验失败，已丢弃并将重传: {e:#}");
                self.incoming = None;
                self.engine.forget_current();
            }
        }
    }

    /// 所有文件到齐：落地为真实文件并写入剪贴板。
    pub(super) fn finish_incoming(&mut self) {
        let transfer = match self.incoming.take() {
            Some(t) => t,
            None => return,
        };

        let items: Vec<(u64, String)> = transfer
            .files
            .iter()
            .map(|f| (f.id, f.name.clone()))
            .collect();

        let paths = match self.deps.cache.materialize(transfer.generation, &items) {
            Ok(p) => p,
            Err(e) => {
                warn!("文件落地失败: {e:#}");
                self.engine.forget_current();
                return;
            }
        };

        // 写入剪贴板，使其可被正常粘贴。
        match self.deps.clipboard.lock() {
            Ok(mut cb) => {
                if let Err(e) = cb.write_files(&paths) {
                    warn!("把文件写入剪贴板失败: {e:#}");
                    self.engine.forget_current();
                    return;
                }
            }
            Err(e) => {
                warn!("获取剪贴板锁失败: {e}");
                return;
            }
        }

        info!(
            "已接收 {} 个文件并放入剪贴板（来自 {}）",
            paths.len(),
            transfer.from
        );

        // 清理更早的落地目录；淘汰超限缓存，但钉住本次引用的内容。
        self.deps
            .cache
            .cleanup_materialized_except(Some(transfer.generation));
        self.deps.cache.evict_to_limit(&transfer.pinned_ids());
    }
}
