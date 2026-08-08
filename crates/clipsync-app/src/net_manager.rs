//! 连接管理：监听入站连接、按地址簿优先级拨号对端，桥接到同步中枢。
//!
//! **路径优选**：拨号时从地址簿取该对端的候选地址，按"同网段直连 → 覆盖网/VPN
//! → 公网"顺序逐个尝试，首个握手成功者胜出并被标记为 `known_good`（下次优先）。
//! 这套逻辑对所有组网方案通用——Tailscale、ZeroTier、WireGuard 或公网端口转发
//! 只是候选地址的不同来源，无需分别适配。
//!
//! **连接方向去重**：两边都会拨号，但同一对设备之间只保留**由 id 较小一方
//! 拨出**的那条（见 `is_canonical`）。早先的做法是"较大一方干脆不拨"，
//! 保证任意两台设备之间恒定只有一条连接。进程内 [`ConnRegistry`] 再兜底一层。
//!
//! **地址互告**：连接建立后立即、并每隔一段时间向对端通告本机全部可达地址，
//! 使对端在当前路径失效时仍握有其它候选。

use std::collections::HashSet;
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clipsync_core::{DeviceId, SyncMessage};
use clipsync_net::crypto::StaticIdentity;
use clipsync_net::local::local_candidates;
use clipsync_net::pairing::PairingRecord;
use clipsync_net::peer::AddrSource;
use clipsync_net::transport::NoiseConnection;
use tracing::{debug, info, warn};

use crate::addrbook::AddrBook;
use crate::filetransfer::OutgoingFiles;
use crate::hub::{HubEvent, HubHandle};
use crate::known_peers::{KnownPeer, KnownPeers};

#[path = "net_pump.rs"]
mod net_pump;

use net_pump::pump;

/// 连接尝试的 TCP 超时：局域网通常毫秒级，覆盖网稍慢；取 3 秒兼顾两者，
/// 避免某个不可达候选拖住后续尝试。
const DIAL_TIMEOUT: Duration = Duration::from_secs(3);
/// 向对端重复通告本机地址的间隔。
pub(super) const ADDR_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(60);
/// 拨号重试的起始间隔。连不上时逐轮加倍，直至 [`DIAL_RETRY_MAX`]。
///
/// **为什么要退避**：原先恒定 3 秒一轮，对端不在线时就一直空转——笔记本合盖
/// 带出门，它会整天以这个节奏挨个尝试每一个候选地址（每个还要等 TCP 超时）。
/// 这既费电又毫无意义：对方不在，再频繁也连不上。
///
/// 退避只在"一轮下来一个都没连上"时推进；**任意一次连接成功即复位**，
/// 所以回到家打开盖子，最迟一个 `DIAL_RETRY_MAX` 就能重新连上。
const DIAL_RETRY_INTERVAL: Duration = Duration::from_secs(3);
/// 退避的上限。
///
/// **从 60 秒下调到 15 秒**，因为一轮拨号的代价变了：原先是逐个候选地址串着
/// `connect_timeout(3s)`，一台设备五个地址最坏空转 15 秒，那样的一轮确实不该
/// 频繁做；改成并发之后，一轮的墙上时间就是一个超时窗口、几个短命 socket。
///
/// 上限直接决定了**对端重新上线后要等多久**——这是实机上最扎眼的那个体感：
/// 打开笔记本，两台设备明明都在线，却要干等十几二十秒。方向去重规定只由
/// id 较小的一方拨号，所以较大的那一方完全被动，等的就是对方这个退避周期。
const DIAL_RETRY_MAX: Duration = Duration::from_secs(15);
/// 每隔多少轮拨号重采样一次本机网段（约 30 秒）。
const REFRESH_NETWORKS_EVERY: u32 = 10;
/// 空闲时的 TCP 读超时。
///
/// **这个值直接就是同步延迟的下限**，因为收发泵是单线程的：它在这里阻塞时，
/// 中枢刚放进发送队列的消息只能干等到超时才发得出去。收到对端消息后也一样,
/// 泵会立刻回到这个阻塞里,完全没给中枢的回复留窗口。
///
/// 曾经取 200ms，实机日志里三次同步一个 **3 字节**的文件，耗时分别是 210ms、
/// 210ms、215ms——传输时间趋近于零，量到的几乎全是这个超时。一次文件同步要
/// 走 `Clip → FileNeed → FileChunk → FileDone` 好几跳，每跳都吃一次。
///
/// 改成 20ms 后最坏延迟降一个数量级，代价是空闲连接每秒多醒 45 次。一次空转
/// 循环只是几个原子读加一次 `read` 系统调用（约 1μs），折合 0.01% 的单核占用，
/// 换十倍的响应速度是划算的。
pub(super) const IDLE_READ_TIMEOUT: Duration = Duration::from_millis(20);
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

    /// 当前已连接的对端数。拨号线程据此判断"网络是不是在好转"。
    fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
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
    /// 配置目录：把对端引荐来的新设备落盘，否则重启就忘了。
    pub config_dir: std::path::PathBuf,
    /// 托盘状态：发文件分块时打个点，图标据此脉冲。
    pub status: crate::tray::TrayStatus,
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

    // 本机地址也记一行。
    //
    // 之前只记了**对端**的地址簿，于是"连不上 192.168.2.225"这种日志无从判断：
    // 是对方在挡，还是这根本就是地址簿里的一条陈货？两边日志各有一半线索却
    // 拼不到一起。把本机地址打出来，对照一下就有答案了。
    let mine = clipsync_net::local::local_candidates(ctx.sync_port);
    if mine.is_empty() {
        warn!("本机一个可用地址都没枚举到，只能等对端主动连入");
    } else {
        info!(
            "本机地址: {}",
            mine.iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

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
            let mut backoff = DIAL_RETRY_INTERVAL;
            loop {
                // 周期性重采样本机网段：覆盖网上线/网络切换后，地址分类
                // （同网段直连 vs 覆盖网）才能保持准确。
                if round % REFRESH_NETWORKS_EVERY == 0 {
                    ctx.addrbook.refresh_local_networks();
                    log_addrbook_state(&ctx);
                }
                round = round.wrapping_add(1);

                // 在取快照**之前**记下版本：本轮进行期间新加的设备也算变化，
                // 立刻再来一轮，而不是等满退避。
                let version_before = ctx.known.version();

                // 入站连上的也算数。
                //
                // 原先只看"本轮我拨通了谁"，于是有个盲点：对端主动连了进来
                // （说明网络明明是好的），我们自己的退避却照涨不误，另一台
                // 设备就得多等好几轮。连接总数变多 = 网络在好转，没有理由
                // 继续退避。
                let connected_before = ctx.registry.len();
                let mut any_connected = false;
                // 有设备还一个地址都没有——多半是引荐刚到、地址正在路上。
                // 这不算"连不上"，不该让退避跟着翻倍。
                let mut awaiting_addrs = false;
                for peer in ctx.known.snapshot() {
                    // **两边都拨。**
                    //
                    // 早先这里有一句"id 较大的一方直接跳过"，代价是它完全被动：
                    // 能多快连上，取决于对方的退避周期走到哪儿了。实机上一台
                    // id 最大的 Mac mini 启动后，两台明明在线的设备分别等了
                    // 9 秒和 28 秒——它自己一个拨号都没发出去过。
                    //
                    // 现在它自己拨；对撞由 `is_canonical` + 让位窗口确定性地
                    // 化解，见 `run_connection`。
                    if ctx.registry.contains(&peer.device) {
                        continue;
                    }
                    if ctx.addrbook.count(&peer.device) == 0 {
                        awaiting_addrs = true;
                        continue;
                    }
                    if dial_peer(&peer, &ctx) {
                        any_connected = true;
                    }
                }

                let table_changed = ctx.known.version() != version_before;
                let gained = ctx.registry.len() > connected_before;
                backoff = next_backoff(
                    backoff,
                    any_connected || gained,
                    awaiting_addrs,
                    table_changed,
                );
                // 睡到退避到期，或设备表一变就提前醒——配对、引荐登记完
                // 立即开拨，不用干等一个退避周期。
                ctx.known.wait_for_change(version_before, backoff);
            }
        })
        .expect("启动拨号线程失败")
}

/// 推进重试间隔。
///
/// 只有**真的试过且没连上**才加倍；下面三种情况一律复位到起始间隔：
///   - `any_connected`：这轮连上了（自己拨通的，或对端拨了进来），说明网络
///     是通的；
///   - `awaiting_addrs`：有设备还没拿到地址（引荐刚到、地址在路上）。这不是
///     "连不上"，把它算作失败会让刚认识的设备白等满一个 [`DIAL_RETRY_MAX`]；
///   - `table_changed`：设备表变过。新设备是新的机会，不该继承此前"连不上"
///     攒下来的长间隔。
///
/// 单独成函数只为能直接测——退避写错（比如忘了复位）的表现是"断网一次之后
/// 就再也不积极重连了"，属于那种平时看不出、真出事才发现的问题。
fn next_backoff(
    current: Duration,
    any_connected: bool,
    awaiting_addrs: bool,
    table_changed: bool,
) -> Duration {
    if any_connected || awaiting_addrs || table_changed {
        DIAL_RETRY_INTERVAL
    } else {
        (current * 2).min(DIAL_RETRY_MAX)
    }
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

/// 按优先级依次尝试该对端的候选地址，首个握手成功者胜出。
///
/// 连接本身交给独立线程跑，本函数即刻返回，好让拨号线程接着去连下一台设备。
/// 返回是否**握手成功过**——拨号线程据此决定要不要退避。
/// 这条连接是不是**规范方向**——由 id 较小的一方拨出。
///
/// 关键性质：**同一条 socket，两端算出的结论必然相同**。各自都知道两个
/// device id，也知道自己是拨出方还是接入方，两个信息合起来就唯一确定了
/// "这条是谁拨的"。有了这条共识，双方就能在不通信的情况下选中同一条连接。
fn is_canonical(local: &DeviceId, peer: &DeviceId, outbound: bool) -> bool {
    if outbound {
        local.as_str() < peer.as_str()
    } else {
        local.as_str() > peer.as_str()
    }
}

/// 非规范方向的连接在登记前让出的时间。
///
/// **为什么需要它**：两边同时拨号时会出现两条 socket。若各自"谁先握完手留谁"，
/// 两端很可能留下不同的那条——A 留自己的出站、B 也留自己的出站，而对方早把
/// 它关了，**两条全死**，五成概率。
///
/// 让非规范的那条晚一步登记，规范那条就总能先占住位置，于是两端**必然**淘汰
/// 同一条。代价只落在兜底路径上：只有本机是 id 较大的一方、且对方没在拨时，
/// 才会实打实等这段时间。
///
/// 取 500 毫秒是留足余量：判断会错只可能发生在"规范连接恰好卡在这个截止点
/// 附近完成"，而同一条 socket 两端完成握手的时间差约为一个 RTT（通常 <100ms）。
/// 窗口远大于 RTT，这个区间就窄到可以忽略；万一真撞上，后果也只是两条都关掉、
/// 下一轮重来，不会卡死。
const NONCANONICAL_YIELD: Duration = Duration::from_millis(500);

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

    // 非规范方向先让一步，好让规范那条抢先登记（见 `NONCANONICAL_YIELD`）。
    if !is_canonical(&ctx.local_device, &peer.device, via.is_some()) {
        std::thread::sleep(NONCANONICAL_YIELD);
    }

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

    let started = Instant::now();
    let result = pump(conn, &peer, ctx, out_rx);

    // 刚连上就断，几乎总是"对端不认识我们"——它认证失败后直接关闭，而我们
    // 这边只看到一句"正常关闭连接"，完全看不出原因。这个现象用户没法自己
    // 诊断（要去翻对端的 pairings.json 才知道），所以直接
    // 把最可能的原因说出来。
    if started.elapsed() < Duration::from_secs(2) {
        info!(
            "与 {} 的连接刚建立就被对方关闭——多半是对端不认识本机：\
             它那边尚未配对，或曾把本机移出设备组",
            peer.name
        );
    }

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

#[path = "net_dial.rs"]
mod dial;

use dial::dial_peer;

#[cfg(test)]
#[path = "net_manager_tests.rs"]
mod tests;
