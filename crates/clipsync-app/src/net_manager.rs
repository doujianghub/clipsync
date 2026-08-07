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
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clipsync_core::{DeviceId, SyncMessage};
use clipsync_net::crypto::StaticIdentity;
use clipsync_net::local::local_candidates;
use clipsync_net::pairing::PairingRecord;
use clipsync_net::peer::AddrSource;
use clipsync_net::transport::{NoiseConnection, RecvOutcome};
use tracing::{debug, info, warn};

use crate::addrbook::AddrBook;
use crate::filecache::CHUNK_SIZE;
use crate::filetransfer::{begin_stream, OutgoingFiles, OutgoingStream};
use crate::hub::{HubEvent, HubHandle};

/// 连接尝试的 TCP 超时：局域网通常毫秒级，覆盖网稍慢；取 3 秒兼顾两者，
/// 避免某个不可达候选拖住后续尝试。
const DIAL_TIMEOUT: Duration = Duration::from_secs(3);
/// 向对端重复通告本机地址的间隔。
const ADDR_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(60);
/// 拨号轮询间隔。
const DIAL_RETRY_INTERVAL: Duration = Duration::from_secs(3);
/// 每隔多少轮拨号重采样一次本机网段（约 30 秒）。
const REFRESH_NETWORKS_EVERY: u32 = 10;
/// 空闲时的 TCP 读超时：较长以降低 CPU 占用。
const IDLE_READ_TIMEOUT: Duration = Duration::from_millis(200);
/// 文件传输中的 TCP 读超时：极短以保证吞吐，同时仍能及时响应对端消息。
const ACTIVE_READ_TIMEOUT: Duration = Duration::from_millis(1);
/// 每轮最多连续发送的文件分块数。
///
/// 取 4（约 1MB）：足够摊薄轮转开销保证吞吐，又能让"剪贴板已更新"的取代
/// 请求在约 1MB 的发送时间内得到响应。
const CHUNKS_PER_ROUND: u32 = 4;

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

/// 已知对端的最小信息（来自配对记录）。
#[derive(Clone)]
pub struct KnownPeer {
    pub device: DeviceId,
    pub name: String,
    pub static_public_key: Vec<u8>,
}

impl From<PairingRecord> for KnownPeer {
    fn from(r: PairingRecord) -> Self {
        Self {
            device: r.device,
            name: r.name,
            static_public_key: r.static_public_key,
        }
    }
}

/// 已配对设备表，可在运行期增补。
///
/// **为什么不是启动时定格的 `Arc<Vec<_>>`**：配对现在可以从托盘发起
/// （「显示配对码…」/「输入配对码…」），配对成功时进程正在运行。若这张表
/// 是启动快照，新配对的设备要**重启程序**才会被拨号线程看见、才会通过入站
/// 认证——用户点完菜单、看到"配对成功"，然后发现什么也同步不了，只能靠
/// 猜出"得重启一下"。
///
/// 读多写极少（几秒一次读、一辈子几次写），用 `Mutex` + 读时克隆即可，
/// 不值得引入 `RwLock` 的复杂度。
#[derive(Clone, Default)]
pub struct KnownPeers {
    inner: Arc<Mutex<Vec<KnownPeer>>>,
}

impl KnownPeers {
    pub fn new(peers: Vec<KnownPeer>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(peers)),
        }
    }

    /// 当前全部已配对设备。
    pub fn snapshot(&self) -> Vec<KnownPeer> {
        self.inner.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn contains(&self, device: &DeviceId) -> bool {
        self.inner.lock().unwrap().iter().any(|p| &p.device == device)
    }

    /// 按静态公钥查找——入站连接的认证依据。
    pub fn find_by_static_key(&self, key: &[u8]) -> Option<KnownPeer> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.static_public_key == key)
            .cloned()
    }

    /// 加入或更新一台设备（按 device id 去重）。
    pub fn upsert(&self, peer: KnownPeer) {
        let mut g = self.inner.lock().unwrap();
        match g.iter_mut().find(|p| p.device == peer.device) {
            Some(existing) => *existing = peer,
            None => g.push(peer),
        }
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
fn upload_limit_of(s: &crate::config::Settings) -> Option<u64> {
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

/// 单连接收发泵：单线程内交替处理"发送中枢广播""发送文件分块""接收对端消息"。
///
/// TCP 读设为超时轮询，使三者互不阻塞，无需共享 Noise 状态（其 `TransportState`
/// 不可跨线程共享）。
///
/// **优先级**：每轮先把中枢的待发消息（文本/图片同步等）全部发出，再发文件分块。
/// 这保证了传大文件期间复制一段文字，对端仍能立即收到——文件传输不会堵住
/// 其它同步。
fn pump(
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

/// 向对端通告本机全部可达地址。
fn announce_addresses(conn: &mut NoiseConnection, ctx: &NetCtx) -> Result<()> {
    let addrs = local_candidates(ctx.sync_port);
    if addrs.is_empty() {
        return Ok(());
    }
    conn.send(&SyncMessage::Addresses { addrs })
        .context("通告本机地址失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str, key: u8) -> KnownPeer {
        KnownPeer {
            device: DeviceId::from_public_key(id.as_bytes()),
            name: id.to_string(),
            static_public_key: vec![key; 32],
        }
    }

    /// 运行期新增的设备必须立刻对拨号与入站认证可见。
    ///
    /// 配对可以从托盘发起，此时进程正在运行。若这张表是启动快照，用户会
    /// 看到"配对成功"却什么都同步不了，且没有任何提示说该重启。
    #[test]
    fn newly_paired_device_is_visible_immediately() {
        let known = KnownPeers::new(vec![peer("a", 1)]);
        let shared = known.clone(); // 拨号线程持有的那一份

        let b = peer("b", 2);
        known.upsert(b.clone());

        assert_eq!(shared.len(), 2, "克隆出的句柄应看到新设备");
        assert!(shared.contains(&b.device), "拨号线程据此决定拨谁");
        assert_eq!(
            shared.find_by_static_key(&b.static_public_key).map(|p| p.name),
            Some("b".to_string()),
            "入站握手据静态公钥认证，查不到就会拒绝这台新配对的设备"
        );
    }

    /// 重复配对同一设备只更新、不产生第二条记录。
    #[test]
    fn upsert_replaces_instead_of_duplicating() {
        let known = KnownPeers::new(vec![peer("a", 1)]);

        let mut renamed = peer("a", 9);
        renamed.name = "改了名的 A".to_string();
        known.upsert(renamed);

        assert_eq!(known.len(), 1, "同一 device id 不应出现两条");
        let got = known.snapshot().pop().unwrap();
        assert_eq!(got.name, "改了名的 A");
        assert_eq!(got.static_public_key, vec![9; 32], "公钥应更新为最新一次配对的");
    }

    #[test]
    fn unknown_static_key_is_not_found() {
        let known = KnownPeers::new(vec![peer("a", 1)]);
        assert!(
            known.find_by_static_key(&[7u8; 32]).is_none(),
            "未配对的公钥必须查不到——这是拒绝陌生连接的依据"
        );
    }
}

/// 把配对记录中保存的对端地址装入地址簿（首次连接的地址来源）。
pub fn seed_addrbook_from_pairings(addrbook: &AddrBook, pairings: &[PairingRecord]) {
    for p in pairings {
        if !p.addrs.is_empty() {
            addrbook.add_addrs(&p.device, p.addrs.iter().copied(), AddrSource::Pairing);
        }
    }
}
