//! 单连接的收发泵。
//!
//! 一个线程内交替做三件事：发中枢广播、发文件分块、收对端消息。TCP 读设为
//! 超时轮询让三者互不阻塞——这样无需跨线程共享 Noise 状态（`TransportState`
//! 本就不可 Send）。
//!
//! **优先级**：每轮先把中枢的待发消息全部发出，再发文件分块。传大文件期间
//! 复制一段文字，对端仍能立即收到。

use std::time::Instant;

use anyhow::{Context, Result};
use clipsync_core::SyncMessage;
use clipsync_net::transport::{NoiseConnection, RecvOutcome};
use tracing::{debug, info, warn};

use super::{
    announce_addresses, upload_limit_of, KnownPeer, NetCtx, ACTIVE_READ_TIMEOUT,
    ADDR_ANNOUNCE_INTERVAL, CHUNKS_PER_ROUND, IDLE_READ_TIMEOUT,
};
use crate::filecache::CHUNK_SIZE;
use crate::filetransfer::{begin_stream, OutgoingStream};
use crate::hub::HubEvent;

/// 单连接收发泵：单线程内交替处理"发送中枢广播""发送文件分块""接收对端消息"。
///
/// TCP 读设为超时轮询，使三者互不阻塞，无需共享 Noise 状态（其 `TransportState`
/// 不可跨线程共享）。
///
/// **优先级**：每轮先把中枢的待发消息（文本/图片同步等）全部发出，再发文件分块。
/// 这保证了传大文件期间复制一段文字，对端仍能立即收到——文件传输不会堵住
/// 其它同步。
pub(super) fn pump(
    mut conn: NoiseConnection,
    peer: &KnownPeer,
    ctx: &NetCtx,
    out_rx: std::sync::mpsc::Receiver<SyncMessage>,
) -> Result<()> {
    // 空闲时用较长读超时（省 CPU）；传文件时用极短超时（保吞吐）；
    // 被限速时按限速器建议等待（避免空转）。
    let mut current_timeout = IDLE_READ_TIMEOUT;
    conn.set_read_timeout(Some(current_timeout))?;

    // 先自报家门。`Hello` 在 v1 就已定义，旧版本能解码后忽略，所以发它总是
    // 安全的；而对端是否**回**一条，正是我们判断它新旧的依据。
    conn.send(&SyncMessage::Hello {
        protocol: clipsync_core::PROTOCOL_VERSION,
        device: ctx.local_device.clone(),
        device_name: whoami(ctx),
    })
    .context("发送 Hello 失败")?;

    announce_addresses(&mut conn, ctx)?;
    let mut last_announce = Instant::now();
    // 对端协议版本；`None` 表示还没收到 Hello（旧版本永远不会发）。
    let mut peer_protocol: Option<u16> = None;
    // 已按哪个版本的设备表引荐过；None 表示还没引荐过。
    let mut introduced_version: Option<u64> = None;

    // 当前正在发送的文件流（同一时刻至多一个）。
    let mut stream: Option<OutgoingStream> = None;
    // 发送限速器：只作用于文件内容，不影响文本/图片同步。
    // 用户可能在托盘里改限速，故记住已应用的设置版本，变化时重建限速器。
    let mut applied_settings = ctx.settings.version();
    let mut limiter =
        crate::ratelimit::RateLimiter::new(upload_limit_of(&ctx.settings.snapshot()));
    if limiter.is_limited() {
        debug!("文件发送限速已启用");
    }

    loop {
        // 限速设置变了就换一个限速器（令牌桶重新计时，不沿用旧配额）。
        let sv = ctx.settings.version();
        if sv != applied_settings {
            limiter = crate::ratelimit::RateLimiter::new(upload_limit_of(&ctx.settings.snapshot()));
            applied_settings = sv;
            debug!("发送限速设置已更新");
        }

        // 本轮是否因限速而未能发满。
        let mut throttled = false;
        // 1) 优先发出中枢的待发消息，确保文本同步不被文件传输拖延。
        loop {
            match out_rx.try_recv() {
                Ok(msg) => conn.send(&msg).context("向对端发送失败")?,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // 2) 推进文件发送。
        if let Some(s) = stream.as_mut() {
            // 剪贴板已更新为别的内容：立即中止，不再浪费带宽。
            // 对端会保留已收字节，将来可断点续传。
            //
            // **手动取回不适用**：对方明确点了「取回」，要的就是那一份，本机
            // 剪贴板后来换成什么与他无关（见 `OutgoingStream::abort_on_supersede`）。
            if s.abort_on_supersede() && !ctx.outgoing.is_current(s.generation()) {
                debug!("剪贴板已更新，中止代际 {} 的文件发送", s.generation());
                conn.send(&SyncMessage::FileAbort {
                    generation: s.generation(),
                })?;
                stream = None;
                ctx.status.clear_transfer();
            } else {
                let mut finished = false;
                for _ in 0..CHUNKS_PER_ROUND {
                    // 限速：问一次本轮允许发多少。令牌不足则本轮不发文件，
                    // 但循环继续——文本同步与取代响应不受限速影响。
                    let budget = limiter.take(CHUNK_SIZE as u64) as usize;
                    if budget == 0 {
                        throttled = true;
                        break;
                    }
                    match s.next_message(budget) {
                        Ok(Some(msg)) => {
                            let done = matches!(msg, SyncMessage::FileDone { .. });
                            conn.send(&msg).context("发送文件分块失败")?;
                            let (sent, total, name) = s.progress();
                            ctx.status.note_transfer(crate::tray::TransferProgress {
                                sending: true,
                                name,
                                done: sent,
                                total,
                            });
                            if done {
                                finished = true;
                                break;
                            }
                        }
                        Ok(None) => {
                            finished = true;
                            break;
                        }
                        Err(e) => {
                            warn!("读取待发送文件失败: {e:#}");
                            finished = true;
                            break;
                        }
                    }
                }
                if finished {
                    stream = None;
                    ctx.status.clear_transfer();
                }
            }
        }

        // 读超时同时充当"节奏控制"：
        //   传输中     → 极短，保吞吐
        //   被限速     → 按限速器建议等待，避免空转耗 CPU
        //   空闲       → 较长，省 CPU
        let desired_timeout = if throttled {
            limiter.suggested_wait().max(ACTIVE_READ_TIMEOUT)
        } else if stream.is_some() {
            ACTIVE_READ_TIMEOUT
        } else {
            IDLE_READ_TIMEOUT
        };
        if desired_timeout != current_timeout {
            conn.set_read_timeout(Some(desired_timeout))?;
            current_timeout = desired_timeout;
        }

        // 3) 周期性通告地址（覆盖网上线/IP 变化后对端能及时学到）。
        if last_announce.elapsed() >= ADDR_ANNOUNCE_INTERVAL {
            announce_addresses(&mut conn, ctx)?;
            last_announce = Instant::now();
        }

        // 收到对端 Hello 后引荐一次；此后**设备表一变就再引荐**。
        //
        // 只引荐一次是不够的：A 与 B 连上时 B 可能还只认识 A，没什么可介绍；
        // 等 B 后来又配了 C，那条已建立的连接若不再引荐，A 就永远不知道 C。
        //
        // 这里曾另有一处"跟着地址通告顺带再引荐一次"的定时兜底。版本驱动之后
        // 它纯属冗余：设备表没变也每 60 秒重发一遍引荐，日志里刷成一片
        // 「向 X 引荐 N 台设备」，把真正的变化淹没了。
        let peers_version = ctx.known.version();
        if peer_protocol.is_some() && introduced_version != Some(peers_version) {
            introduce_peers(&mut conn, ctx, peer, peer_protocol)?;
            introduced_version = Some(peers_version);
        }

        // 4) 接收对端消息。
        match conn.recv_timeout() {
            Ok(RecvOutcome::Message(msg)) => {
                // 文件索取请求在此直接处理：它需要流式读盘，交给中枢会把
                // 整个文件塞进消息通道，既占内存又无法中途取消。
                if let SyncMessage::FileNeed {
                    generation,
                    file_id,
                    offset,
                } = msg
                {
                    match begin_stream(
                        &ctx.outgoing,
                        generation,
                        file_id,
                        offset,
                        ctx.settings.snapshot().compress_transfers,
                    ) {
                        Ok(s) => {
                            debug!("开始发送文件 {file_id:016x}（自偏移 {offset}）");
                            stream = Some(s);
                        }
                        Err(reply) => conn.send(&reply).context("回复文件请求失败")?,
                    }
                } else if let SyncMessage::Hello { protocol, .. } = msg {
                    debug!("对端 {} 协议版本 {}", peer.name, protocol);
                    peer_protocol = Some(protocol);
                } else if let SyncMessage::Peers { peers } = msg {
                    learn_peers(ctx, peer, peers);
                } else if let SyncMessage::Removed { device } = msg {
                    apply_removal(ctx, peer, &device);
                    // 被移出的是自己，或是本连接的对端——这条连接没必要留了。
                    if device == ctx.local_device || device == peer.device {
                        return Ok(());
                    }
                } else {
                    ctx.hub.send(HubEvent::Remote {
                        from: peer.device.clone(),
                        msg,
                    });
                }
            }
            Ok(RecvOutcome::Closed) => {
                info!("对端 {} 正常关闭连接", peer.name);
                return Ok(());
            }
            Ok(RecvOutcome::Timeout) => { /* 正常超时，回到发送检查 */ }
            Err(e) => return Err(e).context("接收对端消息失败"),
        }
    }
}

/// 本机设备名。配对记录里存的是**对端**的名字，本机名只能现取。
fn whoami(ctx: &NetCtx) -> String {
    let _ = ctx;
    crate::device_name::device_name_best_effort()
}

/// 把本机已知的其它设备介绍给对端。
///
/// 不包含对端自己（它当然认识自己），也不包含本机（对端已经连着我们了）。
/// 对端版本低于 2 时静默跳过——那边的枚举里没有 `Peers`，发过去只会让它
/// 解码失败、断开、重连，循环往复。
fn introduce_peers(
    conn: &mut NoiseConnection,
    ctx: &NetCtx,
    peer: &KnownPeer,
    peer_protocol: Option<u16>,
) -> Result<()> {
    if peer_protocol.unwrap_or(0) < 2 {
        return Ok(());
    }
    let peers: Vec<clipsync_core::PeerIntro> = ctx
        .known
        .snapshot()
        .into_iter()
        .filter(|p| p.device != peer.device)
        .map(|p| clipsync_core::PeerIntro {
            addrs: ctx
                .addrbook
                .connect_order(&p.device)
                .into_iter()
                .map(|c| c.addr)
                .collect(),
            device: p.device,
            name: p.name,
            static_public_key: p.static_public_key,
        })
        .collect();
    if peers.is_empty() {
        return Ok(());
    }
    debug!("向 {} 引荐 {} 台设备", peer.name, peers.len());
    conn.send(&SyncMessage::Peers { peers }).context("引荐设备失败")
}

/// 收下对端引荐的设备：登记、落盘、记住地址。
///
/// **信任模型**：只接受**已配对**对端的引荐——能发到这里的连接都通过了
/// Noise 静态密钥认证。也就是说，引荐权等同于"你已经信任了这台设备"，
/// 与它能直接同步你的剪贴板相比，介绍一台新设备并不是更大的权限。
///
/// 已认识的设备只更新地址，不覆盖记录——避免对端用一个同 id 但不同公钥的
/// 条目把已有配对顶掉。
fn learn_peers(ctx: &NetCtx, from: &KnownPeer, peers: Vec<clipsync_core::PeerIntro>) {
    for p in peers {
        // 别把自己学进去。
        if p.device == ctx.local_device {
            continue;
        }
        let is_new = !ctx.known.contains(&p.device);

        // **地址必须先进地址簿，再把设备加进设备表。**
        //
        // `known.upsert` 会当场唤醒拨号线程（见 `KnownPeers::wait_for_change`），
        // 而它醒来第一件事就是查地址簿。反过来写的话，它拿到的是一台一个地址
        // 都没有的设备：拨不出去 → 记为"这轮没连上" → 退避翻倍 → 最长要等
        // 60 秒才重试。实机日志里"经 KPC 认识了 MacBook Pro"到真正连上正好
        // 隔了 60.019 秒，就是撞上了这个。
        //
        // 中间还夹着一次磁盘写入（落盘配对记录），窗口有好几毫秒，拨号线程
        // 几乎必然抢先——这个顺序问题本来就在，是"立刻唤醒"把它从偶发变成
        // 必现。
        if !p.addrs.is_empty() {
            ctx.addrbook.add_addrs(
                &p.device,
                p.addrs.clone(),
                clipsync_net::peer::AddrSource::Peer,
            );
        }

        if is_new {
            info!("经 {} 认识了新设备 {}（{}）", from.name, p.name, p.device);
            ctx.known.upsert(crate::known_peers::KnownPeer {
                device: p.device.clone(),
                name: p.name.clone(),
                static_public_key: p.static_public_key.clone(),
            });
            // 落盘，否则重启就忘了。
            let record = clipsync_net::pairing::PairingRecord {
                device: p.device.clone(),
                name: p.name.clone(),
                static_public_key: p.static_public_key.clone(),
                addrs: p.addrs,
                introduced_by: Some(from.name.clone()),
            };
            if let Err(e) = crate::config::upsert_pairing(&ctx.config_dir, record) {
                warn!("保存引荐来的设备失败: {e:#}");
            }
        }
    }
}

/// 处理对端发来的"移出设备组"。
///
/// 组内任何成员都可以移出任何人——这些本就是同一个人的设备，不设管理员。
/// 消息由已配对且通过 Noise 认证的对端发来，不存在陌生人乱踢的问题。
fn apply_removal(ctx: &NetCtx, from: &KnownPeer, device: &clipsync_core::DeviceId) {
    if device == &ctx.local_device {
        // 自己被移出组了：清空全部配对，安静退出这个组。
        // 不这么做的话，本机会带着一份别人早已作废的名单不停重连。
        info!("{} 把本机移出了设备组，已清空本机的配对记录", from.name);
        for p in ctx.known.snapshot() {
            ctx.known.remove(&p.device);
            ctx.addrbook.forget(&p.device);
            ctx.hub.send(HubEvent::Unpaired { device: p.device });
        }
        if let Err(e) = crate::config::clear_pairings(&ctx.config_dir) {
            warn!("清空配对记录失败: {e:#}");
        }
        return;
    }

    if ctx.known.remove(device) {
        info!("{} 将 {} 移出了设备组", from.name, device);
        ctx.addrbook.forget(device);
        ctx.hub.send(HubEvent::Unpaired {
            device: device.clone(),
        });
        if let Err(e) = crate::config::remove_pairing(&ctx.config_dir, device) {
            warn!("移除配对记录失败: {e:#}");
        }
    }
}
