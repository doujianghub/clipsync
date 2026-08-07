//! 找到正在等待配对的主持方，免去让用户手输 IP。
//!
//! # 为什么需要它
//!
//! 局域网里靠 UDP 组播就能找到对方，但**覆盖网不转发组播**——Tailscale、
//! ZeroTier、WireGuard 这类 L3 overlay 都是如此。组播落空后，原先只剩"请
//! 输入对方 IP"一条路，而这恰恰是本工具最该消灭的那种输入。
//!
//! # 结构：一个探测器 + 若干候选来源
//!
//! 不为每种组网工具各写一套发现逻辑，而是让各来源只负责**产出候选地址**，
//! 再由同一个并发探测器去连配对端口——谁应答谁就是主持方。加一种组网方式
//! 只是多一个来源，探测这一半不用动。
//!
//! 真正的分界线不是"哪家厂商"，而是**接口前缀能不能枚举**：
//!
//! | 组网方式 | 接口前缀 | 能否枚举 |
//! |---|---|---|
//! | 局域网 / ZeroTier / WireGuard / Nebula | 通常 /24 | 能，[`subnet_candidates`] 直接搞定，不必认识厂商 |
//! | Tailscale / NetBird | **/32**，地址空间 100.64.0.0/10 | 不能（400 万个），只能问工具本身要名单 |
//!
//! 所以 [`overlay_tool_candidates`] 只是**最后一道**，且只在网段枚举落空后
//! 才会去起子进程。
//!
//! # 曾考虑但没采用：路由表里的主机路由
//!
//! Tailscale 会给**通信过的**对端装 /32 主机路由，`netstat -rn` 里看得到，
//! 这是完全不认厂商的通用信号。可惜它只覆盖"已经聊过的机器"——而首次配对
//! 恰恰是"从没聊过"。为一个覆盖不到目标场景的信号，在三个平台各写一份
//! `netstat` / `route print` 文本解析，不划算。
//!
//! # 分寸
//!
//! 探测只在用户点了「输入配对码…」之后的那几秒内发生，**从不在后台跑**；
//! 候选总数有上限，网段过大直接跳过。这是"找一台机器"，不是扫网。

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tracing::{debug, info};

/// 单次探测的连接超时。
///
/// 局域网与覆盖网的握手都在毫秒级，300ms 足够；再长只是让"这台不在"这件事
/// 慢一点被确认。
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// 等对方开口的时间。
///
/// 主持方 accept 之后**立刻**发出自己的 PAKE 帧——本机实测 23ms。400ms 足够
/// 容忍覆盖网经中继的一次往返，再长只是让"这不是主持方"这件事慢一点被确认。
/// 开着 TUN 模式代理的机器上每个候选都会连上，这个值直接决定整轮的耗时。
const GREETING_TIMEOUT: Duration = Duration::from_millis(400);

/// 并发探测的线程数。
///
/// 一个 /24 有 253 个候选，128 并发 = 2 轮，最坏 2×(300+400)ms ≈ 1.4 秒，
/// 常见情况远快于此。线程只在这一次探测里活着，且都在等 I/O，栈也调小了，
/// 开销可以忽略。
const PROBE_CONCURRENCY: usize = 128;

/// 探测线程的栈大小。它们只做 connect + peek，用不着默认的 2MiB；
/// 128 个线程按默认栈会白占 256MiB 虚拟地址空间。
const PROBE_STACK: usize = 64 * 1024;

/// 候选地址总数上限。超过就不是"找一台机器"而是扫网了。
const MAX_CANDIDATES: usize = 1024;

/// 可枚举网段的最短前缀：/22 即 1022 台主机。
///
/// 再短（/16 有 6.5 万台）就该老实问用户要地址——扫下去既慢又扎眼。
const MIN_PREFIX_V4: u32 = 22;

/// 调用组网工具的超时。工具本身都是本地查询，正常在百毫秒内返回；
/// 设这个上限只为防住"守护进程挂了、命令一直不返回"把配对流程卡死。
const TOOL_TIMEOUT: Duration = Duration::from_secs(3);

/// 已知的组网工具：只列**地址空间不可枚举**的那几种。
///
/// ZeroTier、WireGuard、Nebula 之类不在这里——它们的虚拟网卡有真实网段，
/// [`subnet_candidates`] 已经覆盖，不必多起一个子进程。
///
/// 每项是 (显示名, 可执行文件候选路径, 参数)。加一种组网工具就加一行。
const OVERLAY_TOOLS: &[(&str, &[&str], &[&str])] = &[
    (
        "Tailscale",
        &[
            "tailscale",
            "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
            "/usr/local/bin/tailscale",
            "/usr/bin/tailscale",
            r"C:\Program Files\Tailscale\tailscale.exe",
        ],
        &["status"],
    ),
    (
        "NetBird",
        &[
            "netbird",
            "/usr/local/bin/netbird",
            "/Applications/NetBird.app/Contents/MacOS/netbird",
            r"C:\Program Files\NetBird\netbird.exe",
        ],
        &["status", "--detail"],
    ),
];

/// 找出正在监听配对端口的主机，按发现顺序返回**已建立的连接**。
///
/// 返回连接而不是地址，是为了不重连一次——重连意味着主持方要多接一个立刻
/// 断开的连接，白白消耗它的失败计数。
///
/// 通常只会有一个结果。返回多个时由调用方依次尝试握手：47685 是个冷僻端口，
/// 但不能断言占着它的一定是 ClipSync。
pub fn find_hosts(port: u16) -> Vec<(SocketAddr, TcpStream)> {
    // 两路候选合并成**一轮**探测，覆盖网的排前面。
    //
    // 分两轮跑过一版：先扫网段，落空了再问组网工具。结果是纯覆盖网场景
    // （两台机器不在同一局域网）每次都要先白等一轮 254 个地址的扫描。
    // 合并之后，前 64 个工作线程一上来就把为数不多的覆盖网对端探完了，
    // 网段那些只是垫在后面慢慢来——先命中先停。
    let mut candidates = overlay_tool_candidates(port);
    let overlay_count = candidates.len();
    candidates.extend(subnet_candidates(port));

    let mut seen = std::collections::HashSet::with_capacity(candidates.len());
    candidates.retain(|a| seen.insert(*a));

    if candidates.is_empty() {
        return Vec::new();
    }
    debug!(
        "探测 {} 个候选（覆盖网 {overlay_count}，网段 {}）",
        candidates.len(),
        candidates.len() - overlay_count
    );
    probe(candidates)
}

/// 本机自己的 IPv4，探测时要跳过。
///
/// 组网工具的输出里第一行往往就是本机——探自己毫无意义，若本机正好也在
/// 主持配对，还会连上自己的监听，白白消耗一次失败计数。
fn own_ipv4() -> std::collections::HashSet<Ipv4Addr> {
    clipsync_net::local::local_networks()
        .v4
        .into_iter()
        .map(|(ip, _)| ip)
        .collect()
}

/// 由本机各网卡的**真实网段**枚举出候选地址。
///
/// 跳过三类：回环与链路本地（对端不可能在那儿）、前缀短于 [`MIN_PREFIX_V4`]
/// 的大网段（枚举代价太高）、以及 /31 /32（没有可用主机位——Tailscale 的
/// utun 就是 /32，这也是它必须走 [`overlay_tool_candidates`] 的原因）。
fn subnet_candidates(port: u16) -> Vec<SocketAddr> {
    let nets = clipsync_net::local::local_networks();
    let mut out = Vec::new();

    for (ip, mask) in nets.v4 {
        if ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() {
            continue;
        }
        let m = u32::from(mask);
        // 掩码必须是连续高位 1，否则 count_ones 算出的前缀没有意义。
        if m == 0 || !m.leading_ones() == 0 && m.count_ones() != m.leading_ones() {
            continue;
        }
        let prefix = m.count_ones();
        if prefix < MIN_PREFIX_V4 || prefix > 30 {
            continue;
        }

        let base = u32::from(ip) & m;
        let broadcast = base | !m;
        let self_ip = u32::from(ip);
        // 掐掉网络号与广播地址，也别探自己。
        for a in (base + 1)..broadcast {
            if a == self_ip {
                continue;
            }
            out.push(SocketAddr::from((Ipv4Addr::from(a), port)));
            if out.len() >= MAX_CANDIDATES {
                info!(
                    "本机网段过大，只探测前 {MAX_CANDIDATES} 个地址；\
                     若对方不在其中，配对时请手动填地址"
                );
                return out;
            }
        }
    }
    out
}

/// 依次问各组网工具要对端地址。
fn overlay_tool_candidates(port: u16) -> Vec<SocketAddr> {
    let own = own_ipv4();
    let mut seen = std::collections::BTreeSet::new();
    for (name, bins, args) in OVERLAY_TOOLS {
        let Some(out) = run_tool(bins, args) else {
            continue;
        };
        let ips = scrape_ipv4(&out);
        if ips.is_empty() {
            continue;
        }
        info!("{name} 报告了 {} 个地址", ips.len());
        seen.extend(ips.into_iter().filter(|ip| !own.contains(ip)));
    }
    seen.into_iter()
        .take(MAX_CANDIDATES)
        .map(|ip| SocketAddr::from((ip, port)))
        .collect()
}

/// 跑一个组网工具，返回它的标准输出；找不到可执行文件或超时则返回 `None`。
fn run_tool(bins: &[&str], args: &[&str]) -> Option<String> {
    for bin in bins {
        let mut cmd = std::process::Command::new(bin);
        cmd.args(args);
        cmd.stdin(std::process::Stdio::null());
        #[cfg(windows)]
        crate::win_util::hidden(&mut cmd);

        // 放到线程里跑并限时：守护进程异常时这些命令可能久久不返回，而配对
        // 流程正等着它。超时后弃线程不管——子进程自己会结束。
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(cmd.output());
        });
        match rx.recv_timeout(TOOL_TIMEOUT) {
            Ok(Ok(o)) if o.status.success() => {
                return Some(String::from_utf8_lossy(&o.stdout).into_owned())
            }
            Ok(Ok(_)) => continue, // 工具在，但报错（多半是没登录）
            Ok(Err(_)) => continue, // 这个路径没有可执行文件，换下一个
            Err(_) => {
                debug!("调用 {bin} 超时，跳过");
                continue;
            }
        }
    }
    None
}

/// 从工具输出里捞出所有像覆盖网地址的 IPv4。
///
/// **不解析 JSON、不认字段名**：各家的输出格式都在变，而"一个 100.64/10 或
/// 私有段的 IPv4 字面量"这个特征不会变。多捞到几个无所谓——探测本来就是
/// 并发的，连不上的 300ms 就超时了；少认一个字段名却可能让整条路失效。
fn scrape_ipv4(text: &str) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    for tok in text.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        let Ok(ip) = tok.parse::<Ipv4Addr>() else {
            continue;
        };
        if is_reachable_peer(ip) && !out.contains(&ip) {
            out.push(ip);
        }
    }
    out
}

/// 这个连接对面是不是真的在等待配对？
///
/// **不能只看"连上了"**。开着 TUN 模式代理（Clash / Surge 之类）的机器上，
/// 代理接管了整张路由表，**连任意地址都会成功**——本机实测连 192.0.2.1
/// （RFC 5737 文档专用、不可能有人监听）都能连上。只认连接成功的话，探测
/// 会在这类机器上把第一个候选当成主持方，然后握手失败，用户看到一句莫名
/// 其妙的错误。
///
/// 判据取自协议本身：主持方 accept 后**立刻**发一帧 PAKE 消息，而帧头是
/// 4 字节大端长度。于是「前两字节为 0 且长度非零」就足以把三类冒充者挡在
/// 外面——一句话不说的代理（读超时）、发别的协议的服务（HTTP 首字节是
/// `H`）、以及长度离谱的垃圾。
///
/// 用 `peek` 而不是 `read`：数据要留在缓冲区里给后面真正的握手用，否则这一
/// 探就把对方的第一帧吃掉了。
fn speaks_pairing(stream: &TcpStream) -> bool {
    let restore = stream.read_timeout().ok().flatten();
    if stream.set_read_timeout(Some(GREETING_TIMEOUT)).is_err() {
        return false;
    }
    let mut head = [0u8; 4];
    let ok = matches!(stream.peek(&mut head), Ok(4))
        && head[0] == 0
        && head[1] == 0
        && u32::from_be_bytes(head) > 0;
    // 探测用的超时不该带进后面的握手。
    let _ = stream.set_read_timeout(restore);
    ok
}

/// 值得一探的对端地址：私有段或 100.64/10（CGNAT，覆盖网常用）。
///
/// 排除回环、链路本地、组播与全零——它们要么是本机，要么根本连不上。
fn is_reachable_peer(ip: Ipv4Addr) -> bool {
    if ip.is_loopback() || ip.is_link_local() || ip.is_multicast() || ip.is_unspecified() {
        return false;
    }
    let o = ip.octets();
    let is_cgnat = o[0] == 100 && (64..128).contains(&o[1]);
    ip.is_private() || is_cgnat
}

/// 并发连接一批地址，返回所有接受了连接的。
///
/// 一旦有人应答就置位停止标志：后续候选不再发起连接，已在途的最多再等
/// [`PROBE_TIMEOUT`]。主持方通常只有一台，没必要把整个网段探完。
fn probe(candidates: Vec<SocketAddr>) -> Vec<(SocketAddr, TcpStream)> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let workers = PROBE_CONCURRENCY.min(candidates.len());
    let queue = Mutex::new(candidates.into_iter());
    let found = Mutex::new(Vec::new());
    let stop = AtomicBool::new(false);

    std::thread::scope(|s| {
        for _ in 0..workers {
            let b = std::thread::Builder::new().stack_size(PROBE_STACK);
            // 线程建不出来就少一个工作线程，不影响正确性——队列共享，
            // 剩下的线程会把活干完。
            let _ = b.spawn_scoped(s, || loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let Some(addr) = queue.lock().unwrap().next() else {
                    return;
                };
                let Ok(stream) = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) else {
                    continue;
                };
                if !speaks_pairing(&stream) {
                    continue;
                }
                stop.store(true, Ordering::Relaxed);
                found.lock().unwrap().push((addr, stream));
            });
        }
    });

    let out = found.into_inner().unwrap();
    for (addr, _) in &out {
        info!("发现等待配对的设备：{addr}");
    }
    out
}

#[cfg(test)]
#[path = "host_probe_tests.rs"]
mod tests;
