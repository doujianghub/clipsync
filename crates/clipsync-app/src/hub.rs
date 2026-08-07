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
    ClipContent, DeviceId, LocalDecision, RemoteDecision, SkipReason, SyncEngine,
    SyncMessage,
};
use tracing::{debug, info, warn};

#[path = "hub_incoming.rs"]
mod hub_incoming;

use hub_incoming::IncomingTransfer;

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
    /// 把某台设备移出设备组，并告知所有还连着的对端。
    ///
    /// `device` 是本机自己时表示"我退出组了"，对端收到后把本机删掉。
    /// 消息也会发给被移出的那台（若它在线），让它自己也清理干净，
    /// 免得它此后一直徒劳重连。
    ///
    /// 不按协议版本区分对端：`Removed` 是 v3 新增的，v2 的对端解不出这个
    /// 变体会断开重连——不好看，但不会出错，而且只在混版运行时出现一次。
    /// 为此在中枢里再记一份每对端的协议版本，代价高于收益。
    AnnounceRemoval { device: DeviceId },
    /// 用户解除了与某设备的配对：立即断开与它的连接。
    ///
    /// 中枢是唯一持有各对端发送通道的地方，移除该通道会让对应连接的收发泵
    /// 读到 `Disconnected` 并退出——解除配对因此当场生效，而不是等对方
    /// 下次重连时才被拒。
    Unpaired { device: DeviceId },
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

/// 中枢运行所需的外部组件。
pub struct HubDeps {
    pub clipboard: Arc<Mutex<ArboardClipboard>>,
    pub addrbook: crate::addrbook::AddrBook,
    pub cache: Arc<FileCache>,
    pub outgoing: OutgoingFiles,
    /// 托盘状态：中枢更新连接数，并读取用户设置的暂停开关。
    pub status: crate::tray::TrayStatus,
    /// 用户设置：托盘改动后中枢据此更新引擎限制。
    pub settings: crate::config::SettingsHandle,
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
    /// 上一次跳过的原因，用于抑制重复的 INFO 提示（见 `log_skip`）。
    last_skip: Option<SkipReason>,
}

fn hub_loop(engine: SyncEngine, deps: HubDeps, rx: Receiver<HubEvent>) {
    let mut st = HubState {
        engine,
        deps,
        peers: HashMap::new(),
        incoming: None,
        last_skip: None,
    };
    // 已应用的设置版本；与句柄里的版本不同即说明用户改过设置。
    let mut applied_settings = st.deps.settings.version();

    for event in rx {
        // 用户可能通过托盘切换了暂停开关；每轮同步给引擎。
        let paused = st.deps.status.is_paused();
        if st.engine.is_paused() != paused {
            st.engine.set_paused(paused);
        }

        // 同理，设置也可能被托盘改过。只比对一个整数，未变则不拿锁。
        //
        // 这里是事件驱动的：空闲时不会执行到这一句，因此改设置后要等下一次
        // 剪贴板变化才应用。这不成问题——检查位于处理 event **之前**，
        // 任何内容被处理前设置一定已是最新；空闲时也没有内容需要它生效。
        let sv = st.deps.settings.version();
        if sv != applied_settings {
            st.engine.set_limits(st.deps.settings.snapshot().to_limits());
            applied_settings = sv;
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
                st.sync_connected_status();
            }
            HubEvent::AnnounceRemoval { device } => {
                let msg = SyncMessage::Removed {
                    device: device.clone(),
                };
                // 发给**所有**对端，包括被移出的那台自己。
                for peer in st.peers.values() {
                    let _ = peer.tx.send(msg.clone());
                }
                info!("已通知 {} 台在线设备：移出 {}", st.peers.len(), device);
            }
            HubEvent::Unpaired { device } => {
                // 丢掉发送通道即切断连接：对应 pump 会读到 Disconnected 并退出。
                if let Some(p) = st.peers.remove(&device) {
                    info!("已解除与 {} ({}) 的配对，连接随之断开", p.name, device);
                }
                st.sync_connected_status();
                if st.incoming.as_ref().map(|t| &t.from) == Some(&device) {
                    debug!("解除配对，放弃来自该设备的文件接收");
                    st.incoming = None;
                    st.engine.forget_current();
                }
            }
            HubEvent::PeerDisconnected { device } => {
                if let Some(p) = st.peers.remove(&device) {
                    info!("对端已断开: {} ({})", p.name, device);
                }
                st.sync_connected_status();
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
    /// 把当前连接情况同步给托盘。
    ///
    /// 连同**在线设备的 ID 集合**一起给出，而不只是一个计数——设备列表要靠它
    /// 标出每台是 ● 还是 ○。中枢是唯一知道谁真正连着的地方。
    fn sync_connected_status(&self) {
        let ids = self
            .peers
            .keys()
            .map(|d| d.to_string())
            .collect::<std::collections::HashSet<_>>();
        self.deps.status.set_connected_ids(ids);
    }

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
                self.log_skip(reason, &content);
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
                "内容 {} 字节，超过单次上限 {} 字节，未同步（可在托盘菜单调整上限）",
                content.byte_size(),
                self.engine.limits().max_bytes
            ),
            SkipReason::KindDisabled => info!(
                "[{}] 未同步：该类型的发送已在托盘菜单中关闭",
                kind_label(content)
            ),
            _ => unreachable!("noteworthy 已限定分支"),
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

            // 这几类都由连接层就地处理，不经中枢：
            //   FileNeed 需流式读盘；Hello/Peers 属于连接自身的协商与设备管理。
            SyncMessage::FileNeed { .. }
            | SyncMessage::Hello { .. }
            | SyncMessage::Peers { .. }
            | SyncMessage::Removed { .. }
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
