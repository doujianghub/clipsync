//! 连接管理：监听入站连接、按地址簿优先级拨号对端，桥接到同步中枢。
//!
//! **路径优选**：拨号时从地址簿取该对端的候选地址，按"同网段直连 → 覆盖网/VPN
//! → 公网"顺序逐个尝试，首个握手成功者胜出并被标记为 `known_good`（下次优先）。
//! 这套逻辑对所有组网方案通用——Tailscale、ZeroTier、WireGuard 或公网端口转发
//! 只是候选地址的不同来源，无需分别适配。
//!
//! **连接方向去重**：仅 device_id 字典序较小的一方主动拨号，另一方只监听，
//! 保证任意两台设备之间恒定只有一条连接。进程内 [`ConnRegistry`] 再兜底一层。
//!
//! **地址互告**：连接建立后立即、并每隔一段时间向对端通告本机全部可达地址，
//! 使对端在当前路径失效时仍握有其它候选。

use std::collections::HashSet;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clipsync_core::{DeviceId, SyncMessage};
use clipsync_net::crypto::StaticIdentity;
use clipsync_net::local::local_candidates;
use clipsync_net::pairing::PairingRecord;
use clipsync_net::peer::AddrSource;
use clipsync_net::transport::NoiseConnection;
use tracing::{debug, info, warn};

use crate::addrbook::AddrBook;
use crate::known_peers::{KnownPeer, KnownPeers};
use crate::filetransfer::OutgoingFiles;
use crate::hub::{HubEvent, HubHandle};

#[path = "net_pump.rs"]
mod net_pump;

use net_pump::pump;

/// 连接尝试的 TCP 超时：局域网通常毫秒级，覆盖网稍慢；取 3 秒兼顾两者，
/// 避免某个不可达候选拖住后续尝试。
const DIAL_TIMEOUT: Duration = Duration::from_secs(3);
/// 向对端重复通告本机地址的间隔。
pub(super) const ADDR_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(60);
/// 拨号轮询间隔。
const DIAL_RETRY_INTERVAL: Duration = Duration::from_secs(3);
/// 每隔多少轮拨号重采样一次本机网段（约 30 秒）。
const REFRESH_NETWORKS_EVERY: u32 = 10;
/// 空闲时的 TCP 读超时：较长以降低 CPU 占用。
pub(super) const IDLE_READ_TIMEOUT: Duration = Duration::from_millis(200);
/// 文件传输中的 TCP 读超时：极短以保证吞吐，同时仍能及时响应对端消息。
pub(super) const ACTIVE_READ_TIMEOUT: Duration = Duration::from_millis(1);
/// 每轮最多连续发送的文件分块数。
///
/// 取 4（约 1MB）：足够摊薄轮转开销保证吞吐，又能让"剪贴板已更新"的取代
/// 请求在约 1MB 的发送时间内得到响应。
pub(super) const CHUNKS_PER_ROUND: u32 = 4;

/// 已连接对端的共享登记表，用于进程内连接去重。
#[derive(Clone, Default)]
pub struct ConnRegistry {
    inner: Arc<Mutex<HashSet<DeviceId>>>,
}

impl ConnRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 尝试登记为"已连接"。成功返回 `true`；已存在（重复连接）返回 `false`。
    fn try_insert(&self, device: &DeviceId) -> bool {
        self.inner.lock().unwrap().insert(device.clone())
    }

    fn remove(&self, device: &DeviceId) {
        self.inner.lock().unwrap().remove(device);
    }

    fn contains(&self, device: &DeviceId) -> bool {
        self.inner.lock().unwrap().contains(device)
    }
}

/// 网络层共享上下文，避免各函数签名过长。
#[derive(Clone)]
pub struct NetCtx {
    pub local_device: DeviceId,
    pub identity: Arc<StaticIdentity>,
    /// 已配对设备表。运行期可被配对流程增补，故每次使用都取快照。
    pub known: KnownPeers,
    pub hub: HubHandle,
    pub registry: ConnRegistry,
    pub addrbook: AddrBook,
    /// 本机待发文件登记表（供对端索取内容）与当前代际号。
    pub outgoing: OutgoingFiles,
    /// 用户设置（限速、压缩开关等）。运行期可被托盘改动，故每次读取快照
    /// 而非在启动时定格——否则用户改了设置要重启才生效。
    pub settings: crate::config::SettingsHandle,
    /// 本机同步监听端口（用于向对端通告自身地址）。
    pub sync_port: u16,
}

/// 从设置取发送限速。`0` 表示不限速（`RateLimiter` 自身也把 0 当无限制，
/// 这里包一层只为让调用点读起来清楚）。
pub(super) fn upload_limit_of(s: &crate::config::Settings) -> Option<u64> {
    Some(s.upload_limit_bytes_per_sec)
}

/// 启动入站监听线程。收到连接后作为 Noise 响应方握手并接入中枢。
pub fn spawn_listener(ctx: NetCtx) -> Result<std::thread::JoinHandle<()>> {
    let listener = TcpListener::bind(("0.0.0.0", ctx.sync_port))
        .with_context(|| format!("监听端口 {} 失败", ctx.sync_port))?;
    info!("正在监听入站连接: 0.0.0.0:{}", ctx.sync_port);

    let handle = std::thread::Builder::new()
        .name("net-listener".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => {
                        let ctx = ctx.clone();
                        // 每个入站连接独立线程握手，避免慢握手阻塞 accept。
                        std::thread::spawn(move || {
                            let conn = match NoiseConnection::accept(s, &ctx.identity.private_key) {
                                Ok(c) => c,
                                Err(e) => {
                                    debug!("入站 Noise 握手失败: {e:#}");
                                    return;
                                }
                            };
                            if let Err(e) = run_connection(conn, &ctx, None) {
                                debug!("入站连接结束: {e:#}");
                            }
                        });
                    }
                    Err(e) => warn!("accept 失败: {e}"),
                }
            }
        })?;
    Ok(handle)
}

/// 启动拨号线程：按地址簿优先级尝试连接尚未连接的对端。
pub fn spawn_dialer(ctx: NetCtx) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("net-dialer".into())
        .spawn(move || {
            let mut round: u32 = 0;
            loop {
                // 周期性重采样本机网段：覆盖网上线/网络切换后，地址分类
                // （同网段直连 vs 覆盖网）才能保持准确。
                if round % REFRESH_NETWORKS_EVERY == 0 {
                    ctx.addrbook.refresh_local_networks();
                    log_addrbook_state(&ctx);
                }
                round = round.wrapping_add(1);

                // 每轮取一次快照：配对流程可能刚加进来一台新设备，
                // 这样无需重启即可开始拨号。
                for peer in ctx.known.snapshot() {
                    // 方向去重：仅由 id 较小的一方主动拨号。
                    if ctx.local_device.as_str() >= peer.device.as_str() {
                        continue;
                    }
                    if ctx.registry.contains(&peer.device) {
                        continue;
                    }
                    dial_peer(&peer, &ctx);
                }
                std::thread::sleep(DIAL_RETRY_INTERVAL);
            }
        })
        .expect("启动拨号线程失败")
}

/// 输出地址簿概况，便于排查"为何连不上"。
fn log_addrbook_state(ctx: &NetCtx) {
    let known = ctx.known.snapshot();
    for (device, cands) in ctx.addrbook.snapshot() {
        let name = known
            .iter()
            .find(|k| k.device == device)
            .map(|k| k.name.as_str())
            .unwrap_or("(未配对)");
        debug!(
            "地址簿 {} ({}): {} 个候选，最优 {:?}",
            name,
            device,
            ctx.addrbook.count(&device),
            cands.first().map(|c| c.addr),
        );
    }
}

/// 按优先级依次尝试该对端的候选地址，首个成功者进入收发循环（阻塞至断开）。
fn dial_peer(peer: &KnownPeer, ctx: &NetCtx) {
    let candidates = ctx.addrbook.connect_order(&peer.device);
    if candidates.is_empty() {
        debug!("对端 {} 暂无候选地址（等待发现或配对信息）", peer.name);
        return;
    }

    for cand in candidates {
        // 逐个尝试期间可能已由入站连接建立，及时收手。
        if ctx.registry.contains(&peer.device) {
            return;
        }
        match dial_addr(peer, cand.addr, ctx) {
            Ok(true) => {
                // 连接已结束（正常断开），本轮到此为止，等下一轮重试。
                return;
            }
            Ok(false) => { /* 该地址不可用，试下一个 */ }
            Err(e) => debug!("连接 {} 失败: {e:#}", cand.addr),
        }
    }
}

/// 尝试单个地址。返回 `Ok(true)` 表示握手成功并已完成一次连接会话。
fn dial_addr(peer: &KnownPeer, addr: SocketAddr, ctx: &NetCtx) -> Result<bool> {
    let stream = match TcpStream::connect_timeout(&addr, DIAL_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            debug!("TCP 连接 {addr} 失败: {e}");
            return Ok(false);
        }
    };
    let conn = NoiseConnection::connect(stream, &ctx.identity.private_key, &peer.static_public_key)
        .context("出站 Noise 握手失败")?;

    info!("已通过 {} 连接 {}", addr, peer.name);
    run_connection(conn, ctx, Some(addr))?;
    Ok(true)
}

/// 连接建立后的公共处理：认证 → 去重登记 → 通知中枢 → 收发泵 → 断开清理。
///
/// `via` 为本次连接实际使用的地址（出站时已知），成功后标记为 `known_good`。
fn run_connection(conn: NoiseConnection, ctx: &NetCtx, via: Option<SocketAddr>) -> Result<()> {
    // 认证：对端静态公钥必须在已配对记录中。
    let remote_static = conn
        .remote_static()
        .ok_or_else(|| anyhow!("握手后无法获取对端静态公钥"))?;
    let peer = ctx
        .known
        .find_by_static_key(&remote_static)
        .ok_or_else(|| anyhow!("对端未配对（静态公钥不在记录中），拒绝连接"))?;

    // 去重：同一对端已有连接则放弃本条。
    if !ctx.registry.try_insert(&peer.device) {
        debug!("对端 {} 已有连接，关闭重复连接", peer.name);
        return Ok(());
    }

    // 记录成功路径，下次优先重试。
    if let Some(addr) = via {
        ctx.addrbook.mark_good(&peer.device, &addr);
    }

    let (out_tx, out_rx) = std::sync::mpsc::channel::<SyncMessage>();
    ctx.hub.send(HubEvent::PeerConnected {
        device: peer.device.clone(),
        name: peer.name.clone(),
        tx: out_tx,
    });
    info!("已认证并连接对端: {} ({})", peer.name, peer.device);

    let result = pump(conn, &peer, ctx, out_rx);

    ctx.registry.remove(&peer.device);
    ctx.hub.send(HubEvent::PeerDisconnected {
        device: peer.device.clone(),
    });
    result
}

/// 向对端通告本机全部可达地址。
pub(super) fn announce_addresses(conn: &mut NoiseConnection, ctx: &NetCtx) -> Result<()> {
    let addrs = local_candidates(ctx.sync_port);
    if addrs.is_empty() {
        return Ok(());
    }
    conn.send(&SyncMessage::Addresses { addrs })
        .context("通告本机地址失败")
}

/// 把配对记录中保存的对端地址装入地址簿（首次连接的地址来源）。
pub fn seed_addrbook_from_pairings(addrbook: &AddrBook, pairings: &[PairingRecord]) {
    for p in pairings {
        if !p.addrs.is_empty() {
            addrbook.add_addrs(&p.device, p.addrs.iter().copied(), AddrSource::Pairing);
        }
    }
}
