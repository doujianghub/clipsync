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

    announce_addresses(&mut conn, ctx)?;
    let mut last_announce = Instant::now();

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
            if !ctx.outgoing.is_current(s.generation()) {
                debug!("剪贴板已更新，中止代际 {} 的文件发送", s.generation());
                conn.send(&SyncMessage::FileAbort {
                    generation: s.generation(),
                })?;
                stream = None;
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
