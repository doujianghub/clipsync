//! 配对命令行流程。
//!
//! 配对需要一条临时 TCP 通道让两台设备跑 PAKE 握手。为"极简配置"，采用：
//!   - **接受方**（`clipsync pair --host`）：生成配对码并显示，在配对端口监听，
//!     等对方连入后跑 `run_pairing`，成功则保存配对记录。
//!   - **发起方**（`clipsync pair <ip> <code>`）：连接对方配对端口，输入配对码，
//!     跑 `run_pairing`，成功则保存配对记录。
//!
//! 二者对称调用 `clipsync_net::pairing_handshake::run_pairing`。配对端口与同步
//! 端口分开，避免与常规同步连接混淆。M3 起可用 mDNS 免去手填 IP。

use clipsync_core::{tf, tprintln};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clipsync_net::crypto::StaticIdentity;
use clipsync_net::pairing::PairingCode;
use clipsync_net::pairing_handshake::{run_pairing, LocalPairingInfo, WrongCode};

use tracing::debug;

use crate::config;

/// 配对使用的 TCP 端口（与同步端口区分）。
pub const PAIRING_PORT: u16 = 47_685;

/// 解析用户输入：纯配对码，或 `配对码@地址`。
///
/// 返回 `(配对码, 地址)`；地址为 `None` 时调用方自动查找对方。
/// 无法识别为合法配对码时返回 `None`。
///
/// 带地址的写法是**手动出口**，不再主动示人：自动发现覆盖到绝大多数情况，
/// 让每个用户都先读一遍 IP 是本末倒置。命令行仍然接受它。
pub fn parse_pairing_input(input: &str) -> Option<(PairingCode, Option<String>)> {
    let s = input.trim();
    match s.rsplit_once('@') {
        // 从右往左切：IPv6 字面量里没有 @，但地址部分可能含冒号与方括号。
        Some((code, host)) => {
            let host = host.trim();
            if host.is_empty() {
                return None;
            }
            Some((PairingCode::parse(code)?, Some(host.to_string())))
        }
        None => Some((PairingCode::parse(s)?, None)),
    }
}

/// 本机全部可达的 IPv4 地址，按"对方最可能连得通"的顺序排好并标注类别。
///
/// **为什么要列全，而不是只给"最合适"的那一个**：哪个地址通，取决于**对方**
/// 在哪张网上——本机无从知道。原先只挑一个（优先覆盖网），碰上对方只在
/// 局域网里，那个地址就是死的，用户还以为程序给错了。列全之后由人来挑，
/// 这件事人比程序清楚。
///
/// 只列 IPv4：要人念、人敲的场合，IPv6 那一长串既难念又易错；真需要时
/// `clipsync addrs` 里有全部。
fn local_ipv4_lines(sync_port: u16) -> Vec<String> {
    // **不能用 `peer::classify`**：那是拿来判断**对端**地址是否与本机同网段的，
    // 而本机自己的地址永远"在自己的网段里"，问它必得「局域网」——实测三张
    // 网卡（局域网、Tailscale、代理 utun）被一律标成局域网，等于没标。
    // 这里按地址段本身判断，与 `clipsync addrs` 同一套口径。
    let mut rows: Vec<(u8, String)> = clipsync_net::local::local_candidates(sync_port)
        .into_iter()
        .filter_map(|sa| match sa.ip() {
            std::net::IpAddr::V4(v4) => Some((v4, sa)),
            std::net::IpAddr::V6(_) => None,
        })
        // 掐掉 198.18.0.0/15：RFC 2544 的基准测试段，公网上永不可路由，
        // 而 Clash/Surge 这类 TUN 模式代理正是拿它做假 IP。它会被认成"公网
        // 地址"列出来，用户挑中就是死路一条。
        .filter(|(v4, _)| {
            let o = v4.octets();
            !(o[0] == 198 && (18..20).contains(&o[1]))
        })
        .map(|(v4, sa)| {
            let o = v4.octets();
            let (rank, label) = if o[0] == 100 && (64..128).contains(&o[1]) {
                (1, "覆盖网")
            } else if v4.is_private() {
                (0, "局域网")
            } else {
                (2, "公网")
            };
            (rank, format!("{}（{label}）", fmt_host(&sa)))
        })
        .collect();
    // 局域网排最前，与连接时「同网段直连 → 覆盖网 → 公网」的优选顺序一致，
    // 免得两处给出的次序打架。
    rows.sort_by_key(|(r, _)| *r);
    rows.into_iter().map(|(_, s)| s).collect()
}

/// 把地址列表排成缩进的几行；一个都没有时返回 `None`。
pub fn addr_block(sync_port: u16) -> Option<String> {
    let lines = local_ipv4_lines(sync_port);
    if lines.is_empty() {
        return None;
    }
    Some(
        lines
            .iter()
            .map(|l| format!("    {l}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// 主持配对的有效期。/// 主持配对的有效期。到期自动结束：释放端口、停止组播宣告。
///
/// **为什么必须有这个**：配对码是一次性凭证，"永远有效"既是安全问题，更是
/// 一个必现的功能故障——见 [`host`] 的说明。
///
/// **为什么是 180 秒而不是 60 秒**：挡住在线猜测的是
/// [`MAX_FAILED_ATTEMPTS`]，不是这个窗口。实测（`measure_online_guessing_rate`）
/// 主持方能承受 **818 次错误猜测/秒**——串行 accept 循环就是穷举速率——
/// 所以没有次数上限时，1 万种组合在 60 秒里必被跑完；有了 5 次上限，无论
/// 窗口是 60 秒还是 180 秒，被猜中的概率都是 5/10000。
///
/// 换句话说，缩短窗口买不到安全，却实实在在地卡住了流程：实机上"在 A 上点
/// 出码 → 走到 B → 打开托盘菜单 → 敲四位数字"这一串就超过了 60 秒，等探测
/// 跑起来 A 那边早已不听，用户看到的是一句莫名其妙的"没找到设备"。
///
/// 期间**关掉配对码窗口不影响监听**——弹窗跑在独立线程里，会话只在超时、
/// 成功、或连错 3 次时结束。
pub const HOST_SESSION_TIMEOUT: Duration = Duration::from_secs(180);

/// 一次主持期间允许的**猜测**次数。
///
/// **整个 4 位码方案就靠它撑着。** 离线穷举确实不可能（SPAKE2 要求每猜一次
/// 都走完一轮握手），但**在线**穷举很快：实测 818 次/秒
/// （`measure_online_guessing_rate`），1 万种组合十几秒就跑完了。没有这个
/// 上限，4 位码等于没有。
///
/// 5 次 → 单会话被猜中 5/10000（0.05%）。用户自己敲错还剩四次机会，宽松。
///
/// **计数是每会话独立的**：一次成功就结束会话并返回，下次点「显示配对码」
/// 从零开始，早先的失败不会累积过来。
///
/// **只有真的发来 PAKE 且验证失败才算一次**（见 `WrongCode`）。连上就断不
/// 计数——否则自动发现去探一批地址，等于自己把自己的会话探挂；顺带也堵掉
/// 一个骚扰：任何人反复连一下就能让你配不上对。
const MAX_FAILED_ATTEMPTS: u32 = 5;

/// accept 的轮询间隔。listener 设为非阻塞后按此节奏检查超时与取消。
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 会话被主动取消——用户点了「换个配对码」。
///
/// 单独一个类型而不是靠错误文案区分：调用方要据此决定"立刻开新一轮"还是
/// "弹一个配对失败"，把这个判断挂在字符串匹配上迟早会因为改文案而失灵
/// （`WrongCode` 也是同样的道理）。
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("已取消当前配对会话")
    }
}

impl std::error::Error for Cancelled {}

/// 作为接受方主持配对：生成并显示配对码，等待对方连入。
///
/// 同时向局域网宣告"我在等待配对"，使对方**无需手输 IP**——直接
/// `clipsync pair <配对码>` 即可。跨网络时才需要用到下方打印的地址。
///
/// `show_dialog`：是否额外弹窗显示配对码。从托盘触发时必须为 `true`
/// （GUI 启动没有终端，`println!` 的内容用户看不到）；命令行下为 `false`，
/// 避免多弹一个窗打断用户。
/// 返回配对成功的记录，供调用方登记到运行中的已配对设备表——从托盘发起
/// 配对时进程正在运行，不登记就要重启才生效。
///
/// # 会话有明确的终点
///
/// 早先的实现是 `loop { listener.accept() }`：**没有超时、没有取消、除了配对
/// 成功没有任何退出路径**。命令行下这没问题（`clipsync pair --host` 是短命
/// 进程，Ctrl+C 就结束了），但从托盘发起时进程是常驻的，后果是一个必现故障：
///
/// > 用户点「显示配对码」，看一眼码，关掉窗口（对方还没准备好，很常见）。
/// > 弹窗返回后线程径直走进 accept 死循环永远卡住，`listener` 永不析构，
/// > 端口 47685 被永久占用。再点一次菜单 → `bind` 失败 →
/// > Windows 报 `os error 10048`、macOS 报 `os error 48`。**重启程序才能恢复。**
/// > 更糟的是 announcer 在 bind **之前**启动，每失败一次就多泄漏一个线程，
/// > 持续向局域网广播"我在等待配对"。
///
/// 现在会话在四种情况下结束，任一发生都会释放端口并停止组播宣告：
///   1. 配对成功；
///   2. 超过 [`HOST_SESSION_TIMEOUT`]；
///   3. 失败尝试达到 [`MAX_FAILED_ATTEMPTS`]；
///   4. `cancel` 被置起——用户点了「换个配对码」，返回 [`Cancelled`]。
///
/// 调用方（托盘）另有单例控制，见 `pairing_ui` 的 `PairingHostSlot`——重复点击
/// 不会再撞上"端口已被占用"，而是复用当前会话、重新显示同一个配对码。
///
/// `on_code` 在开始等待前调用一次，交出配对码与**到期时刻**：托盘据此在菜单里
/// 显示实时倒计时，弹窗据此写出「有效至 21:47:30」。UI 一律由调用方负责——
/// 本模块只管协议，不弹窗。
pub fn host(
    dir: &Path,
    identity: &StaticIdentity,
    device_name: &str,
    sync_port: u16,
    cancel: &AtomicBool,
    on_code: impl FnOnce(&str, Instant),
) -> Result<clipsync_net::pairing::PairingRecord> {
    let code = PairingCode::generate();

    // 先 bind 再宣告：bind 是唯一可能失败的一步，若失败，宣告线程就不该
    // 存在。反过来（原实现）会在每次 bind 失败时都留下一个停不掉的宣告线程。
    let listener = TcpListener::bind(("0.0.0.0", PAIRING_PORT))
        .with_context(|| format!("监听配对端口 {PAIRING_PORT} 失败"))?;
    // 非阻塞 + 轮询：`accept()` 一旦阻塞就无法被超时或取消唤醒，而这正是
    // 上面那个故障的直接成因。与 net_manager 收发泵的做法一致。
    listener
        .set_nonblocking(true)
        .context("设置配对监听为非阻塞失败")?;

    // 向局域网宣告等待配对（失败不致命，退化为需手输 IP）。
    // 守卫**必须**持有到函数结束——丢弃即停止宣告，这正是我们要的语义。
    let announcer =
        clipsync_net::discovery::spawn_pairing_announcer(device_name.to_string(), PAIRING_PORT)
            .ok();
    let announcing = announcer.is_some();

    let deadline = Instant::now() + HOST_SESSION_TIMEOUT;
    // 交给调用方之后它可能立刻弹窗——那个窗口会一直挡到用户点掉，所以必须
    // 起在别的线程上（调用方的事），而监听此刻已就绪，不会错过任何连接。
    on_code(code.as_str(), deadline);

    println!();
    tprintln!("  配对码： {}", "  Pairing code: {}", code);
    println!();
    if announcing {
        tprintln!(
            "  在另一台设备上运行（同一局域网内，无需输入 IP）：",
            "  On the other machine run (same LAN, no IP needed):"
        );
        println!("      clipsync pair {}", code);
        println!();
    }
    print_manual_hint(&code, sync_port);
    println!(
        "  正在等待对方连接（{} 秒内有效）…",
        HOST_SESSION_TIMEOUT.as_secs()
    );

    let mut failures = 0u32;

    loop {
        let Some((mut stream, addr)) = accept_until(&listener, deadline, cancel)? else {
            if cancel.load(Ordering::Acquire) {
                return Err(anyhow::Error::new(Cancelled));
            }
            anyhow::bail!(
                "配对已超时（{} 秒内无人连入），请重新发起配对",
                HOST_SESSION_TIMEOUT.as_secs()
            );
        };
        tprintln!(
            "  对方已连接：{}，正在协商…",
            "  Peer connected: {}, negotiating…",
            addr
        );

        // 握手本身也要有超时，否则一个连上就不说话的对端能把会话挂死，
        // 端口又回到"占着不放"的老问题上。
        let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
        let _ = stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT));

        let local = local_info(identity, device_name, sync_port);
        match run_pairing(&mut stream, &code, &local) {
            Ok(record) => {
                config::upsert_pairing(dir, record.clone())?;
                tprintln!(
                    "  ✓ 配对成功：{} ({})",
                    "  ✓ Paired: {} ({})",
                    record.name,
                    record.device
                );
                print_learned_addrs(&record);
                return Ok(record);
            }
            Err(e) => {
                // 连接层面的失败（探测器连一下就断、网络抖动）不算猜测。
                if e.downcast_ref::<WrongCode>().is_none() {
                    debug!("一个连接未完成握手（不计入猜测次数）: {e:#}");
                    continue;
                }
                failures += 1;
                tprintln!("  ✗ 配对码不对：{e:#}", "  ✗ Wrong pairing code: {e:#}");
                if failures >= MAX_FAILED_ATTEMPTS {
                    anyhow::bail!(
                        "配对码连错 {failures} 次，已结束本次配对以防猜测；\
                         请重新发起（会生成新的配对码）"
                    );
                }
                println!(
                    "  仍在等待（配对码不变，还可重试 {} 次）…",
                    MAX_FAILED_ATTEMPTS - failures
                );
            }
        }
    }
}

/// 单次配对握手的读写超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// 轮询等待一个入站连接，直到 `deadline` 或被取消。
///
/// `Ok(None)` 表示到期或被取消——两者由调用方查 `cancel` 区分。listener 必须
/// 已设为非阻塞：`accept()` 一旦阻塞，超时与取消都叫不醒它。
fn accept_until(
    listener: &TcpListener,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<Option<(TcpStream, std::net::SocketAddr)>> {
    loop {
        match listener.accept() {
            Ok((stream, addr)) => {
                // 交给调用方的流要回到阻塞模式：握手是同步读写，非阻塞流
                // 会让它一路撞 WouldBlock。
                stream
                    .set_nonblocking(false)
                    .context("恢复配对连接为阻塞模式失败")?;
                return Ok(Some((stream, addr)));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(e) => return Err(e).context("接受配对连接失败"),
        }
    }
}

/// 打印跨网络场景下需要手动指定的地址。
fn print_manual_hint(code: &PairingCode, sync_port: u16) {
    let addrs = clipsync_net::local::local_candidates(sync_port);
    if addrs.is_empty() {
        return;
    }
    tprintln!(
        "  若两台设备不在同一局域网，在对方设备上运行：",
        "  If they are not on the same LAN, run this on the other machine:"
    );
    for sa in &addrs {
        println!("      clipsync pair {} {}", fmt_host(sa), code);
    }
    println!();
}

/// 把 SocketAddr 的主机部分格式化为可直接用于命令行的形式。
fn fmt_host(sa: &std::net::SocketAddr) -> String {
    match sa.ip() {
        // IPv6 需要方括号，避免与端口分隔符混淆。
        std::net::IpAddr::V6(v6) => format!("[{v6}]"),
        std::net::IpAddr::V4(v4) => v4.to_string(),
    }
}

/// 配对码弹窗的正文。
///
/// `pub` 是为了让「会话进行中重复点菜单」那条路复用同一份文案——此前那里
/// 另写了一段更短的，结果是"关掉窗口再打开，地址就没了"，用户以为程序把
/// 信息弄丢了。同一件事只该有一份文案。
///
/// **有效期写成绝对时刻**（「有效至 21:47:30」）而不是「还有 2 分 47 秒」：
/// 这个窗口是系统原生模态窗，显示出来就改不了文字了，相对时间从显示的那一刻
/// 起就在撒谎。实时倒计时在托盘菜单里，那儿本来就每 200ms 转一圈。
/// 取不到系统时钟时退回说总时长——那仍然是真话，只是粗一些。
pub fn code_dialog_body(code: &str, sync_port: u16, expires_in: Duration) -> String {
    let validity = match crate::wallclock::hms_after(expires_in) {
        Some(at) => tf!("有效至 {}", "valid until {}", at),
        None => tf!(
            "{} 分钟内有效",
            "valid for {} minutes",
            HOST_SESSION_TIMEOUT.as_secs() / 60
        ),
    };
    let mut s = tf!(
        "配对码  {}\n\n\
         在对方设备上选「输入配对码…」，输入这四位。\n\
         {}，配对成功即失效。",
        "Pairing code  {}\n\n\
         On the other machine choose \"Enter pairing code…\" and type these four \
         digits.\n\
         This code is {} and stops working once pairing succeeds.",
        code,
        validity
    );
    if let Some(block) = addr_block(sync_port) {
        s.push_str(&tf!(
            "\n\n若对方提示需要地址，挑一个与它同网段的：\n{}",
            "\n\nIf it asks for an address, pick the one on its subnet:\n{}",
            block
        ));
    }
    s
}

/// 作为发起方完成配对。
///
/// `host_ip` 为 `None` 时在局域网中自动发现等待配对的设备。
pub fn join(
    dir: &Path,
    identity: &StaticIdentity,
    device_name: &str,
    host_ip: Option<&str>,
    code_str: &str,
    sync_port: u16,
) -> Result<clipsync_net::pairing::PairingRecord> {
    let mut last = None;
    for mut stream in connect_hosts(host_ip)? {
        match join_on(&mut stream, dir, identity, device_name, code_str, sync_port) {
            Ok(r) => return Ok(r),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("没有可用的连接")))
}

/// 连到主持方，返回一批**已建立**的连接，调用方依次尝试握手。
///
/// 给了地址就直连；没给则走两级自动发现，都不必让用户输 IP：
///   1. **局域网组播**——同网段最快，零探测；
///   2. **主动探测**（[`crate::host_probe`]）——覆盖网不转发组播，只能反过来
///      由本机去连一批候选地址，谁应答谁就是主持方。
///
/// 返回多个是因为占着 47685 的不一定就是 ClipSync；通常只有一个。
///
/// 与握手**分成两步**，是为了让调用方能区分"连不上"和"码不对"：前者值得
/// 换条路再试，后者再试多少次都一样。
pub fn connect_hosts(host_ip: Option<&str>) -> Result<Vec<TcpStream>> {
    if let Some(ip) = host_ip {
        let addr = format!("{ip}:{PAIRING_PORT}");
        tprintln!("  正在连接 {addr} …", "  Connecting to {addr}…");
        let s = TcpStream::connect(&addr).with_context(|| format!("连接 {addr} 失败"))?;
        return Ok(vec![s]);
    }

    // 组播命中的地址交给同一个探测器验证——同机可能同时占着 47685 的别的
    // 服务，而且一台主机会给出多个地址（逐网卡宣告），挨个连才知道哪条通。
    match discover_host() {
        Ok(addrs) => {
            tprintln!(
                "  正在连接 {} 个组播发现的地址…",
                "  Connecting to {} address(es) found via multicast…",
                addrs.len()
            );
            let found = crate::host_probe::probe_candidates(addrs);
            if !found.is_empty() {
                return Ok(found.into_iter().map(|(_, s)| s).collect());
            }
            debug!("组播发现的地址都连不上，改为主动探测");
        }
        Err(e) => debug!("局域网组播未发现主持方，改为主动探测: {e:#}"),
    }

    tprintln!("  正在探测可达设备…", "  Probing for reachable devices…");
    let found = crate::host_probe::find_hosts(PAIRING_PORT);
    if found.is_empty() {
        anyhow::bail!(
            "没能自动找到等待配对的设备。\n  \
             请确认对方正显示着配对码（60 秒内有效），\n  \
             或改用带地址的写法：clipsync pair <配对码>@<对方IP>"
        );
    }
    Ok(found.into_iter().map(|(_, s)| s).collect())
}

/// 在已建立的连接上完成配对握手并落盘。
pub fn join_on(
    stream: &mut TcpStream,
    dir: &Path,
    identity: &StaticIdentity,
    device_name: &str,
    code_str: &str,
    sync_port: u16,
) -> Result<clipsync_net::pairing::PairingRecord> {
    let code =
        PairingCode::parse(code_str).with_context(|| format!("配对码格式非法: {code_str}"))?;

    tprintln!("  已连接，正在协商…", "  Connected, negotiating…");
    let local = local_info(identity, device_name, sync_port);
    let record = run_pairing(stream, &code, &local).context("配对握手失败")?;

    config::upsert_pairing(dir, record.clone())?;
    tprintln!(
        "  ✓ 配对成功：{} ({})",
        "  ✓ Paired: {} ({})",
        record.name,
        record.device
    );
    print_learned_addrs(&record);
    Ok(record)
}

/// 在局域网中查找正在等待配对的设备，返回它的**全部**可连地址。
///
/// 一台主机会给出多个地址：信标逐网卡发送，局域网口与覆盖网口各来一份。
/// 全都返回，由调用方挨个试——一条走不通还有下一条。
fn discover_host() -> Result<Vec<std::net::SocketAddr>> {
    use clipsync_net::discovery::discover_pairing_hosts;

    tprintln!(
        "  正在局域网中查找等待配对的设备…",
        "  Looking for a device waiting to pair…"
    );
    let hosts =
        discover_pairing_hosts(std::time::Duration::from_secs(2)).context("局域网配对发现失败")?;

    match hosts.len() {
        0 => anyhow::bail!(
            "未在局域网中找到等待配对的设备。\n  \
             请确认对方正显示着配对码；\n  \
             若两台设备不在同一局域网，本机会自动改为主动探测"
        ),
        1 => {
            let h = &hosts[0];
            tprintln!(
                "  找到设备：{}（{} 个地址）",
                "  Found: {} ({} address(es))",
                h.device_name,
                h.addrs.len()
            );
            Ok(h.addrs.clone())
        }
        _ => {
            // 现在这条分支才真的表示"多台**不同**设备"——按设备名归并之前，
            // 同一台机器的多个网卡就会误入此处，同机 e2e 一跑就露馅。
            let mut msg = String::from("局域网中有多台设备在等待配对，请指定其一：\n");
            for h in &hosts {
                if let Some(a) = h.addrs.first() {
                    msg.push_str(&format!(
                        "      clipsync pair <配对码>@{}   # {}\n",
                        a.ip(),
                        h.device_name
                    ));
                }
            }
            anyhow::bail!(msg)
        }
    }
}

/// 展示配对时学到的对端地址，让用户直观看到可用路径。
fn print_learned_addrs(record: &clipsync_net::pairing::PairingRecord) {
    if record.addrs.is_empty() {
        return;
    }
    tprintln!(
        "  已记录对方 {} 个可达地址：",
        "  Recorded {} reachable address(es):",
        record.addrs.len()
    );
    for a in &record.addrs {
        println!("      {a}");
    }
}

/// 本机在配对中提供的信息：身份 + 全部可达地址。
///
/// 地址由网卡枚举得来，因此 Tailscale/ZeroTier/WireGuard 等覆盖网的虚拟网卡
/// 地址会自动包含在内——配对完成后即使离开局域网也能直接连通。
fn local_info(identity: &StaticIdentity, device_name: &str, sync_port: u16) -> LocalPairingInfo {
    LocalPairingInfo {
        device_id: identity.device_id().to_string(),
        name: device_name.to_string(),
        static_public_key: identity.public_key.clone(),
        addrs: clipsync_net::local::local_candidates(sync_port),
    }
}

#[cfg(test)]
#[path = "pairing_cli_tests.rs"]
mod tests;
