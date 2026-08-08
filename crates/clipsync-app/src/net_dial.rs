//! 出站建连：从地址簿里挑一条路连上对端。
//!
//! 与 `net_manager` 的分工：那边管连接的生命周期（监听、认证、去重、收发泵），
//! 这边只回答一个问题——**这台设备现在能从哪条路连上**。
//!
//! 核心是两条规则：候选地址**并发**试（一轮只花一个超时窗口），但**按优先级
//! 挑**（同网段直连比覆盖网快一个数量级，先到者胜出会经常选中更差的路）。

use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clipsync_net::transport::NoiseConnection;
use clipsync_net::peer::Candidate;
use tracing::{debug, info};

use super::{run_connection, NetCtx, DIAL_TIMEOUT};
use crate::known_peers::KnownPeer;

/// 一轮拨号里同时试探的地址数上限。
///
/// 候选通常只有三五个，这个数只是防失控的护栏。
const DIAL_FANOUT: usize = 16;

/// 每个探测线程的栈。只做一次 `connect_timeout`，用不着默认的 2 MiB。
const DIAL_STACK: usize = 64 * 1024;

pub(super) fn dial_peer(peer: &KnownPeer, ctx: &NetCtx) -> bool {
    let candidates = ctx.addrbook.connect_order(&peer.device);
    if candidates.is_empty() {
        debug!("对端 {} 暂无候选地址（等待发现或配对信息）", peer.name);
        return false;
    }
    if ctx.registry.contains(&peer.device) {
        return true;
    }

    // **并发建连，再按优先级挑**。
    //
    // 原先是逐个 `connect_timeout(3s)` 串着试：一台设备五个候选地址，光是等
    // 超时最坏就要 15 秒，而拨号线程还要挨个设备走一遍——实机日志里"启动后
    // 隔了 29 秒才连上第二台"就是这么攒出来的。地址多半是覆盖网/公网/局域网
    // 各来一份，其中不通的那几个每个都要白等满一个超时。
    //
    // 并发之后一轮的墙上时间就是**一个**超时窗口。这与配对探测（`host_probe`）
    // 早就在用的做法一致——那边 128 路并发探 262 个地址，几百毫秒出结果。
    //
    // **仍然按优先级挑**，不是"谁先连上用谁"：同网段直连比覆盖网快一个数量级，
    // 而 TCP 建连的快慢跟后续吞吐没什么关系，让先到者胜出会经常选中更差的路。
    let tried: Vec<Candidate> = candidates.into_iter().take(DIAL_FANOUT).collect();
    let addrs: Vec<SocketAddr> = tried.iter().map(|c| c.addr).collect();

    // 优先级最高的那个连上就立刻用它，握手失败再退而求其次。
    let mut skip = 0;
    while let Some((i, stream)) = connect_best(&addrs[skip..]) {
        let cand = &tried[skip + i];
        if ctx.registry.contains(&peer.device) {
            return true; // 期间已由入站连接建立
        }
        match handshake_and_run(peer, stream, cand.addr, ctx) {
            Ok(()) => return true,
            Err(e) => debug!("与 {} 握手失败: {e:#}", cand.addr),
        }
        skip += i + 1;
    }
    false
}

/// 尝试单个地址。返回 `Ok(true)` 表示握手成功、连接已交给独立线程。
///
/// **连接必须跑在独立线程里**：`run_connection` 会一直阻塞到断开。早先它直接
/// 在拨号线程里跑，后果是拨通第一台设备后整个拨号线程就卡在那条连接上，
/// **其余设备永远拨不到**。两台设备时看不出来（本来就只有一个对端），三台
/// 才暴露：A 连上 B 之后再也没去连 C。入站监听一直是每连接一线程，出站这边
/// 漏了。
/// 同时对一批地址发起 TCP 连接，返回与输入等长的结果（`None` 表示没连上）。
///
/// 保持下标对应而不是只返回成功的那些：调用方要按**优先级**挑，而优先级就是
/// 输入顺序。
fn connect_best(addrs: &[SocketAddr]) -> Option<(usize, TcpStream)> {
    race_by_priority(addrs, |addr| {
        TcpStream::connect_timeout(&addr, DIAL_TIMEOUT)
            .inspect_err(|e| debug!("TCP 连接 {addr} 失败: {e}"))
            .ok()
    })
}

/// 并发跑 `f`，返回**优先级最高**的那个成功结果及其下标。
///
/// 「优先级」就是输入顺序（地址簿已按同网段直连 → 覆盖网 → 公网排好）。
///
/// **一旦答案不可能再变好就立刻返回**，不等剩下的跑完。这条很关键：地址簿里
/// 常年躺着几个连不上的地址（换过网的旧局域网段、代理的假 IP、拿不到路由的
/// IPv6），它们每个都要走满一个 `DIAL_TIMEOUT`。若等齐再挑，哪怕最优地址
/// 5 毫秒就连上了，整轮照样是 3 秒——实机上两个实例同时启动、明明都在
/// 127.0.0.1 上，也整整花了 3 秒，就是这么来的。
///
/// 判据是"前缀已定"：从头扫，遇到还没报告的就继续等，遇到成功的就返回。
/// 不用凭感觉设宽限时间。
///
/// 线程是**游离**的，不用 `thread::scope`——作用域会在退出时 join 全部线程，
/// 那就等于没有提前返回。晚到的结果发给已丢弃的接收端，socket 随之关闭，
/// 而 `DIAL_TIMEOUT` 本身就是它们的寿命上限。
fn race_by_priority<T: Send + 'static>(
    addrs: &[SocketAddr],
    f: fn(SocketAddr) -> Option<T>,
) -> Option<(usize, T)> {
    if addrs.is_empty() {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    for (i, &addr) in addrs.iter().enumerate() {
        let tx = tx.clone();
        let _ = std::thread::Builder::new()
            .name("net-dial".into())
            .stack_size(DIAL_STACK)
            .spawn(move || {
                let _ = tx.send((i, f(addr)));
            });
    }
    drop(tx);

    // 已经连上一个之后，最多再给排在它前面的地址多少时间。
    //
    // 光靠"前缀已定"是不够的。那个判据假设每个地址迟早会有结论，可地址簿里
    // 常年躺着一批**永远不会有结论**的地址——换过网段的旧局域网地址就是典型：
    // 对端明明还在 192.168.2.x，本机却搬到了 192.168.3.x，包发出去石沉大海，
    // 只能走满 `DIAL_TIMEOUT`。实机日志里 Tailscale 地址 15 毫秒就握手成功，
    // 却因为前面排着两个这样的地址，整整等了 3 秒才用上。
    //
    // 宽限期不是凭感觉拍的，它基于一个可靠的不对称：**连得通的地址响应时间
    // 以毫秒计，连不通的以秒计**。同网段直连通常 10 毫秒内完成，300 毫秒足够
    // 它抢在前面；而连不通的地址无论给多久都不会变好。所以这段等待只会在
    // "高优先级确实可达"时被用到，代价封顶 300 毫秒，收益是砍掉数秒的空等。
    const PRIORITY_GRACE: Duration = Duration::from_millis(300);

    /// 前缀已全部有结论时，最靠前的那个成功下标。
    fn settled_best<T>(got: &[Option<Option<T>>]) -> Option<usize> {
        for (i, slot) in got.iter().enumerate() {
            match slot {
                None => return None,    // 还没报告，前缀未定
                Some(None) => continue, // 失败，看下一个
                Some(Some(_)) => return Some(i),
            }
        }
        None
    }

    let mut got: Vec<Option<Option<T>>> = (0..addrs.len()).map(|_| None).collect();
    // 兜底期限：所有探测本身都受 DIAL_TIMEOUT 约束，多给一点余量防线程起不来。
    let deadline = Instant::now() + DIAL_TIMEOUT + Duration::from_secs(1);
    // 首个成功结果到手的时刻，宽限期从这里开始算。
    let mut first_hit: Option<Instant> = None;

    loop {
        // 前缀全部有定论、且撞到一个成功的 —— 答案已不可能更好。
        if let Some(i) = settled_best(&got) {
            return got[i].take().flatten().map(|v| (i, v));
        }
        // 前缀还没定，但手里已经有能用的了：宽限期一过就别再等了。
        if let Some(since) = first_hit {
            if since.elapsed() >= PRIORITY_GRACE {
                let i = got
                    .iter()
                    .position(|s| matches!(s, Some(Some(_))))
                    .expect("first_hit 已置位，必有一个成功结果");
                debug!("高优先级地址迟迟无果，改用第 {i} 个已连上的地址");
                return got[i].take().flatten().map(|v| (i, v));
            }
        }

        // 等到下一个结果，或等到宽限期／兜底期限先到。
        let mut left = deadline.saturating_duration_since(Instant::now());
        if let Some(since) = first_hit {
            left = left.min(PRIORITY_GRACE.saturating_sub(since.elapsed()));
        }
        if left.is_zero() {
            return None;
        }
        match rx.recv_timeout(left) {
            Ok((i, r)) => {
                let hit = r.is_some();
                got[i] = Some(r);
                if hit && first_hit.is_none() {
                    first_hit = Some(Instant::now());
                }
            }
            // 宽限期到点：回到循环顶去取那个已成功的结果。
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if first_hit.is_some() => {}
            Err(_) => return None, // 全部线程结束或兜底超时
        }
    }
}

fn handshake_and_run(
    peer: &KnownPeer,
    stream: TcpStream,
    addr: SocketAddr,
    ctx: &NetCtx,
) -> Result<()> {
    let conn = NoiseConnection::connect(stream, &ctx.identity.private_key, &peer.static_public_key)
        .context("出站 Noise 握手失败")?;

    info!("已通过 {} 连接 {}", addr, peer.name);
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name(format!("net-conn-{}", peer.device))
        .spawn(move || {
            if let Err(e) = run_connection(conn, &ctx, Some(addr)) {
                debug!("出站连接结束: {e:#}");
            }
        })
        .context("启动连接线程失败")?;
    Ok(())
}

#[cfg(test)]
#[path = "net_dial_tests.rs"]
mod tests;
