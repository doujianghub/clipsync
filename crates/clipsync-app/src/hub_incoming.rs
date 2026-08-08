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

/// 一批**挂起待取**的文件：超过自动取回上限，等用户在托盘上点一下。
///
/// 只留一份。剪贴板本来就只有一格，同时挂着两份"待取"既无处显示，语义上也
/// 说不通——新的文件复制就是对旧的取代。
pub(super) struct PendingFetch {
    pub(super) from: DeviceId,
    pub(super) generation: u64,
    pub(super) files: Vec<FileMeta>,
    pub(super) total: u64,
}

/// 一次进行中的文件接收。
pub(super) struct IncomingTransfer {
    pub(super) generation: u64,
    pub(super) from: DeviceId,
    pub(super) files: Vec<FileMeta>,
    /// 是否由用户在托盘上手动点「取回」发起。
    ///
    /// 自动传输失败了不必打扰人——下次复制同一份内容还会再来一遍。手动那次
    /// 不同：人刚点了一下，什么反馈都没有只会让人以为按钮坏了。
    pub(super) manual: bool,
    /// 与 `files` 等长：各文件内容是否已完整落入缓存。
    pub(super) done: Vec<bool>,
    /// 本批文件的总字节数与已到手字节数，供托盘报进度。
    ///
    /// 累计着算而不是每块去问缓存：`have_bytes` 要按文件查一遍，放在每个
    /// 分块的路径上就是白白的开销。
    pub(super) total_bytes: u64,
    pub(super) got_bytes: u64,
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

/// 这批文件该不该现在就拉？
///
/// 两条规则，单独成纯函数是为了能不搭一整套中枢就把边界测掉：
///   1. 不超过上限就拉——这是绝大多数情况，行为与从前完全一致；
///   2. **缓存里整份都有的一律拉**，不管多大。那是零传输：来回切换同一个
///      大文件、或断线重连后重复通告都会走这条路，此时还要人多点一下纯属
///      多余，而"取回"这个词也名不副实——根本没什么要取。
fn should_auto_fetch(total: u64, limit: u64, fully_cached: bool) -> bool {
    total <= limit || fully_cached
}

impl HubState {
    /// 对端通告了一批文件：**由本机决定**要不要现在就拉。
    ///
    /// 这是"自动取回上限"落地的地方，也是这套语义的要点——发送方无从知道
    /// 本机的网络与磁盘状况，"值不值得拉"只有收的人判断得了。超限的挂起，
    /// 在托盘上点一下再拉（见 [`fetch_pending`](Self::fetch_pending)）。
    pub(super) fn offer_incoming_files(
        &mut self,
        from: DeviceId,
        generation: u64,
        files: Vec<FileMeta>,
    ) {
        let total: u64 = files.iter().map(|f| f.size).sum();
        let limit = self.deps.settings.snapshot().auto_fetch_bytes as u64;
        let cached = files
            .iter()
            .all(|f| self.deps.cache.is_complete(f.id, f.size));

        if should_auto_fetch(total, limit, cached) {
            self.clear_pending();
            self.begin_incoming_files(from, generation, files, false);
            return;
        }

        // 挂起。进行中的接收一并作废——对端剪贴板已经不是那份内容了，
        // 它自己也会中止；已收字节留在缓存里，将来仍可续传。
        self.incoming = None;
        // **告诉引擎"这份内容并没有进我的剪贴板"**。
        //
        // `on_remote_clip` 判定 Apply 时就把内容哈希记成了当前状态，而我们这条
        // 路根本没写剪贴板。不撤销的话，对方过一会儿再复制同一个文件，就会被
        // 当成重复内容直接跳过——托盘上那一行再也不会出现，用户只会觉得
        // "又复制了一遍怎么没反应"。
        self.engine.forget_current();
        info!(
            "收到 {} 个文件共 {} 字节，超过自动取回上限 {} 字节，已挂起等待手动取回",
            files.len(),
            total,
            limit
        );
        self.pending = Some(PendingFetch {
            from,
            generation,
            files,
            total,
        });
        self.publish_pending();
    }

    /// 用户在托盘上点了「取回」。
    pub(super) fn fetch_pending(&mut self) {
        let Some(p) = self.pending.take() else {
            debug!("没有待取回的文件，忽略");
            return;
        };
        // 对方不在线就原样放回去：`FileNeed` 根本发不出去，硬着头皮开一个
        // 永远等不到分块的接收，只会让托盘上的那一行凭空消失。
        if !self.peers.contains_key(&p.from) {
            info!("待取文件的来源设备 {} 当前不在线，暂不取回", p.from);
            self.pending = Some(p);
            self.publish_pending();
            crate::hub::notify(clipsync_core::t!(
                "对方当前不在线。\n\n等它上线后再点一次「取回」——\n只要它没有再复制别的文件，这一份就还在。",
                "That device is offline right now.\n\nClick \"Fetch\" again once it is \
                 back — the files are still waiting, as long as it has not copied \
                 something else in the meantime."
            ));
            return;
        }
        self.publish_pending();
        info!(
            "手动取回 {} 个文件（共 {} 字节，来自 {}）",
            p.files.len(),
            p.total,
            p.from
        );
        self.begin_incoming_files(p.from, p.generation, p.files, true);
    }

    /// 传输被打断（对端掉线）——手动那次放回待取队列。
    ///
    /// 自动传输不必管：下次复制同一份内容还会再来一遍。手动这次是人专门点过
    /// 的，就此消失等于白点，而已收字节还在缓存里，重连后点一下就能续上。
    pub(super) fn requeue_if_manual(&mut self) {
        let Some(t) = self.incoming.take() else {
            return;
        };
        if !t.manual {
            return;
        }
        info!("手动取回被中断，放回待取队列（已收部分保留，可续传）");
        self.pending = Some(PendingFetch {
            from: t.from,
            generation: t.generation,
            total: t.total_bytes,
            files: t.files,
        });
        self.publish_pending();
    }

    /// 丢弃待取项并同步给托盘。
    pub(super) fn clear_pending(&mut self) {
        if self.pending.take().is_some() {
            self.publish_pending();
        }
    }

    /// 把待取项的摘要交给托盘（菜单项、角标、悬停提示都读它）。
    fn publish_pending(&self) {
        self.deps.status.set_pending(self.pending.as_ref().map(|p| {
            crate::tray::PendingFetchInfo {
                from: p.from.to_string(),
                first_name: p
                    .files
                    .first()
                    .map(|f| f.name.clone())
                    .unwrap_or_else(|| "文件".into()),
                count: p.files.len(),
                total: p.total,
            }
        }));
    }

    /// 开始接收一批文件：先查缓存，只索取缺失的部分。
    pub(super) fn begin_incoming_files(
        &mut self,
        from: DeviceId,
        generation: u64,
        files: Vec<FileMeta>,
        manual: bool,
    ) {
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
            manual,
            total_bytes: total,
            got_bytes: cached_bytes,
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
            info!(
                "开始接收 {} 个文件（共 {} 字节）",
                transfer.files.len(),
                total
            );
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
            self.deps.status.clear_transfer();
            let _ = from;
            return;
        }

        // 报进度。名字取当前这个文件——多文件是逐个传的，报总数反而看不出在动。
        if let Some(t) = self.incoming.as_mut() {
            t.got_bytes = t.got_bytes.saturating_add(plain.len() as u64);
            let name = t
                .index_of(file_id)
                .map(|i| t.files[i].name.clone())
                .unwrap_or_else(|| "文件".into());
            self.deps
                .status
                .note_transfer(crate::tray::TransferProgress {
                    sending: false,
                    name,
                    done: t.got_bytes,
                    total: t.total_bytes,
                });
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

#[cfg(test)]
#[path = "hub_incoming_tests.rs"]
mod tests;
