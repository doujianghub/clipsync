//! 同步中枢：串联剪贴板与多个对端连接。
//!
//! 中枢是唯一持有 `SyncEngine` 的地方，把各类事件串行化后处理，天然无锁竞争：
//!   - **本地剪贴板变化** → 引擎判定 → 广播给所有对端。
//!   - **远端剪贴板消息** → 引擎判定 → 写入本地剪贴板。
//!   - **文件内容分块** → 落入缓存 → 全部到齐后落地并写入剪贴板。
//!
//! **文件传输的取代语义**：同一时刻只保留一个"接收中"的传输。收到更新的剪贴板
//! 内容时，旧传输立即作废——因为对端剪贴板里已经不是那份内容了，继续等待只会
//! 让两端不一致。但**已收到的字节会保留在缓存里**，将来若再次复制同一文件即可
//! 断点续传，甚至完整命中零传输。

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use clipsync_clip::{ArboardClipboard, Clipboard};
use clipsync_core::{
    ClipContent, DeviceId, FileMeta, LocalDecision, RemoteDecision, SyncEngine, SyncMessage,
};
use tracing::{debug, info, warn};

use crate::filecache::FileCache;
use crate::filetransfer::OutgoingFiles;

/// 汇聚到中枢的事件。
pub enum HubEvent {
    /// 本地剪贴板发生变化。
    Local {
        content: ClipContent,
        sensitive: bool,
        /// 内容为文件时各文件的本机路径，供对端索取内容时读取。
        file_paths: Vec<std::path::PathBuf>,
    },
    /// 收到某对端的消息。
    Remote { from: DeviceId, msg: SyncMessage },
    /// 某对端连接建立。
    PeerConnected {
        device: DeviceId,
        name: String,
        tx: Sender<SyncMessage>,
    },
    /// 某对端断开。
    PeerDisconnected { device: DeviceId },
}

/// 中枢句柄：向中枢投递事件。可克隆，分发给各线程。
#[derive(Clone)]
pub struct HubHandle {
    tx: Sender<HubEvent>,
}

impl HubHandle {
    pub fn send(&self, event: HubEvent) {
        // 中枢线程若已退出，发送失败可忽略（进程正在关闭）。
        let _ = self.tx.send(event);
    }
}

/// 每个已连接对端的运行时状态。
struct Peer {
    name: String,
    tx: Sender<SyncMessage>,
}

/// 一次进行中的文件接收。
struct IncomingTransfer {
    generation: u64,
    from: DeviceId,
    files: Vec<FileMeta>,
    /// 与 `files` 等长：各文件内容是否已完整落入缓存。
    done: Vec<bool>,
}

impl IncomingTransfer {
    fn all_done(&self) -> bool {
        self.done.iter().all(|d| *d)
    }

    fn index_of(&self, file_id: u64) -> Option<usize> {
        self.files.iter().position(|f| f.id == file_id)
    }

    fn pinned_ids(&self) -> Vec<u64> {
        self.files.iter().map(|f| f.id).collect()
    }
}

/// 中枢运行所需的外部组件。
pub struct HubDeps {
    pub clipboard: Arc<Mutex<ArboardClipboard>>,
    pub addrbook: crate::addrbook::AddrBook,
    pub cache: Arc<FileCache>,
    pub outgoing: OutgoingFiles,
    /// 托盘状态：中枢更新连接数，并读取用户设置的暂停开关。
    pub status: crate::tray::TrayStatus,
}

/// 启动中枢线程，返回投递句柄。
pub fn start_hub(
    engine: SyncEngine,
    deps: HubDeps,
) -> (HubHandle, std::thread::JoinHandle<()>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("sync-hub".into())
        .spawn(move || hub_loop(engine, deps, rx))
        .expect("启动中枢线程失败");
    (HubHandle { tx }, handle)
}

struct HubState {
    engine: SyncEngine,
    deps: HubDeps,
    peers: HashMap<DeviceId, Peer>,
    incoming: Option<IncomingTransfer>,
}

fn hub_loop(engine: SyncEngine, deps: HubDeps, rx: Receiver<HubEvent>) {
    let mut st = HubState {
        engine,
        deps,
        peers: HashMap::new(),
        incoming: None,
    };

    for event in rx {
        // 用户可能通过托盘切换了暂停开关；每轮同步给引擎。
        let paused = st.deps.status.is_paused();
        if st.engine.is_paused() != paused {
            st.engine.set_paused(paused);
        }

        match event {
            HubEvent::Local {
                content,
                sensitive,
                file_paths,
            } => st.on_local(content, sensitive, file_paths),
            HubEvent::Remote { from, msg } => st.on_remote(from, msg),
            HubEvent::PeerConnected { device, name, tx } => {
                info!("对端已连接: {} ({})", name, device);
                st.peers.insert(device, Peer { name, tx });
                st.deps.status.set_connected(st.peers.len());
            }
            HubEvent::PeerDisconnected { device } => {
                if let Some(p) = st.peers.remove(&device) {
                    info!("对端已断开: {} ({})", p.name, device);
                }
                st.deps.status.set_connected(st.peers.len());
                // 正在从该对端接收的传输就此中断；保留已收字节以便将来续传。
                if st.incoming.as_ref().map(|t| &t.from) == Some(&device) {
                    debug!("对端断开，接收中的文件传输暂停（已收部分保留待续传）");
                    st.incoming = None;
                    st.engine.forget_current();
                }
            }
        }
    }
}

impl HubState {
    /// 处理本地剪贴板变化：判定后广播给所有对端。
    fn on_local(
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
                for peer in self.peers.values() {
                    let _ = peer.tx.send(msg.clone());
                }
                info!("已同步 [{kind}] {size} 字节 → {} 台设备", self.peers.len());
            }
            LocalDecision::Skip(reason) => {
                debug!("本地变化跳过 ({:?})", reason);
            }
        }
    }

    /// 处理远端消息。
    fn on_remote(&mut self, from: DeviceId, msg: SyncMessage) {
        match msg {
            SyncMessage::Clip {
                seq,
                content_hash,
                content,
                ..
            } => self.on_remote_clip(from, seq, content_hash, content),

            SyncMessage::FileChunk {
                generation,
                file_id,
                offset,
                data,
                compressed,
                plain_len,
            } => self.on_file_chunk(&from, generation, file_id, offset, &data, compressed, plain_len),

            SyncMessage::FileDone {
                generation,
                file_id,
                content_hash,
            } => self.on_file_done(&from, generation, file_id, content_hash),

            SyncMessage::FileAbort { generation } => {
                if self.matches_incoming(generation) {
                    info!("对端已取消文件传输（其剪贴板已更新），已收部分保留待续传");
                    self.incoming = None;
                    self.engine.forget_current();
                }
            }

            SyncMessage::FileUnavailable {
                generation,
                file_id,
                reason,
            } => {
                if self.matches_incoming(generation) {
                    warn!("对端无法提供文件（id={file_id:016x}）: {reason}");
                    self.incoming = None;
                    self.engine.forget_current();
                }
            }

            SyncMessage::Addresses { addrs } => {
                let n = addrs.len();
                self.deps
                    .addrbook
                    .add_addrs(&from, addrs, clipsync_net::peer::AddrSource::Peer);
                debug!("已更新 {} 的地址（{} 条通告）", from, n);
            }

            // FileNeed 由连接层直接处理（需流式读盘，不经中枢）。
            SyncMessage::FileNeed { .. }
            | SyncMessage::Hello { .. }
            | SyncMessage::Ping
            | SyncMessage::Pong => {}
        }
    }

    fn matches_incoming(&self, generation: u64) -> bool {
        self.incoming
            .as_ref()
            .map(|t| t.generation == generation)
            .unwrap_or(false)
    }

    /// 收到对端的剪贴板内容。
    fn on_remote_clip(
        &mut self,
        from: DeviceId,
        seq: u64,
        content_hash: u64,
        content: ClipContent,
    ) {
        match self.engine.on_remote_clip(&content, content_hash) {
            RemoteDecision::Skip(reason) => {
                debug!("远端内容跳过 ({:?})", reason);
            }
            RemoteDecision::Apply => match content {
                ClipContent::Files(metas) => self.begin_incoming_files(from, seq, metas),
                other => {
                    // 文本/图片可立即落地。
                    self.apply_to_clipboard(&other, content_hash, &from);
                }
            },
        }
    }

    /// 开始接收一批文件：先查缓存，只索取缺失的部分。
    fn begin_incoming_files(&mut self, from: DeviceId, generation: u64, files: Vec<FileMeta>) {
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
    fn on_file_chunk(
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

    fn on_file_done(
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
    fn finish_incoming(&mut self) {
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

    /// 把文本/图片写入本地剪贴板（含防回环登记）。
    fn apply_to_clipboard(&mut self, content: &ClipContent, content_hash: u64, from: &DeviceId) {
        // 关键防回环：写入前登记预期回声哈希，使随之而来的本地变化被识别为
        // 回声而不再广播。
        self.engine.expect_echo(content_hash);
        match self.deps.clipboard.lock() {
            Ok(mut cb) => {
                if let Err(e) = cb.write(content) {
                    warn!("写入本地剪贴板失败: {e:#}");
                } else {
                    info!("已应用来自 {} 的 [{}] {} 字节", from, kind_label(content), content.byte_size());
                }
            }
            Err(e) => warn!("获取剪贴板锁失败: {e}"),
        }
    }

    fn send_to(&self, device: &DeviceId, msg: SyncMessage) {
        if let Some(p) = self.peers.get(device) {
            let _ = p.tx.send(msg);
        }
    }
}

fn kind_label(content: &ClipContent) -> &'static str {
    match content.kind() {
        clipsync_core::ContentKind::Text => "文本",
        clipsync_core::ContentKind::Image => "图片",
        clipsync_core::ContentKind::Files => "文件",
    }
}
