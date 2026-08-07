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

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clipsync_net::crypto::StaticIdentity;
use clipsync_net::pairing::PairingCode;
use clipsync_net::pairing_handshake::{run_pairing, LocalPairingInfo};

use crate::config;

/// 配对使用的 TCP 端口（与同步端口区分）。
pub const PAIRING_PORT: u16 = 47_685;

/// 主持配对的有效期。到期自动结束：释放端口、停止组播宣告。
///
/// **为什么必须有这个**：配对码是一次性凭证，"永远有效"既是安全问题，更是
/// 一个必现的功能故障——见 [`host`] 的说明。3 分钟足够两台设备走完流程，
/// 又不至于让用户看完码去忙别的时还一直开着。
pub const HOST_SESSION_TIMEOUT: Duration = Duration::from_secs(180);

/// 一次主持期间允许的失败尝试次数。
///
/// 配对码是 30^6 ≈ 7.3 亿组合，靠 SPAKE2 保证每猜一次都要走一轮完整握手。
/// 但原实现失败后无限重试，等于给在线猜测开了不限次数的窗口——`pairing.rs`
/// 的注释声称有"错误锁定"，实际并不存在。超过此数即结束会话，用户重新点一次
/// 菜单会拿到**新的**配对码。
const MAX_FAILED_ATTEMPTS: u32 = 5;

/// accept 的轮询间隔。listener 设为非阻塞后按此节奏检查超时与取消。
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(200);

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
/// 现在会话在三种情况下结束，任一发生都会释放端口并停止组播宣告：
///   1. 配对成功；
///   2. 超过 [`HOST_SESSION_TIMEOUT`]；
///   3. 失败尝试达到 [`MAX_FAILED_ATTEMPTS`]。
///
/// 调用方（托盘）另有单例控制，见 `main.rs` 的 `PairingHostSlot`——重复点击
/// 不会再撞上"端口已被占用"，而是复用当前会话、重新显示同一个配对码。
/// `on_code` 在配对码生成后、开始等待前调用一次，让调用方（托盘的单例槽位）
/// 知道当前会话用的是哪个码——用户重复点菜单时要把同一个码再显示一遍。
pub fn host(
    dir: &Path,
    identity: &StaticIdentity,
    device_name: &str,
    sync_port: u16,
    show_dialog: bool,
    on_code: impl FnOnce(&PairingCode),
) -> Result<clipsync_net::pairing::PairingRecord> {
    let code = PairingCode::generate();
    on_code(&code);

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

    println!();
    println!("  配对码： {}", code);
    println!();
    if announcing {
        println!("  在另一台设备上运行（同一局域网内，无需输入 IP）：");
        println!("      clipsync pair {}", code);
        println!();
    }
    print_manual_hint(&code, sync_port);
    println!("  正在等待对方连接（{} 秒内有效）…", HOST_SESSION_TIMEOUT.as_secs());

    // 从托盘启动时没有终端，上面的 println! 用户一个字也看不到——不弹窗
    // 等于"点了菜单没反应"。故在**开始监听之后**再弹：弹窗会阻塞本线程直到
    // 用户点掉，此时监听已就绪，对方可以随时连入，不会错过连接。
    //
    // 注意这里在后台线程调用，不涉及主线程 UI 约束（见 dialog 模块说明）。
    if show_dialog {
        crate::dialog::show_info_and_copy(
            "ClipSync 配对",
            &dialog_body(&code, sync_port, announcing),
            &code.to_string(),
        );
    }

    // 计时从**弹窗关闭后**开始：弹窗会阻塞本线程，用户盯着码看多久都不该
    // 算进有效期里，否则慢慢念给对方听的过程能把会话耗光。
    let deadline = Instant::now() + HOST_SESSION_TIMEOUT;
    let mut failures = 0u32;

    loop {
        let Some((mut stream, addr)) = accept_until(&listener, deadline)? else {
            anyhow::bail!(
                "配对已超时（{} 秒内无人连入），请重新发起配对",
                HOST_SESSION_TIMEOUT.as_secs()
            );
        };
        println!("  对方已连接：{addr}，正在协商…");

        // 握手本身也要有超时，否则一个连上就不说话的对端能把会话挂死，
        // 端口又回到"占着不放"的老问题上。
        let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
        let _ = stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT));

        let local = local_info(identity, device_name, sync_port);
        match run_pairing(&mut stream, &code, &local) {
            Ok(record) => {
                config::upsert_pairing(dir, record.clone())?;
                println!("  ✓ 配对成功：{} ({})", record.name, record.device);
                print_learned_addrs(&record);
                return Ok(record);
            }
            Err(e) => {
                failures += 1;
                println!("  ✗ 本次配对未成功：{e:#}");
                if failures >= MAX_FAILED_ATTEMPTS {
                    anyhow::bail!(
                        "配对失败 {failures} 次，已结束本次配对以防猜测；\
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

/// 轮询等待一个入站连接，直到 `deadline`。
///
/// `Ok(None)` 表示到期仍无人连入。listener 必须已设为非阻塞。
fn accept_until(
    listener: &TcpListener,
    deadline: Instant,
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
                if Instant::now() >= deadline {
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
    println!("  若两台设备不在同一局域网，在对方设备上运行：");
    for sa in &addrs {
        println!("      clipsync pair {} {}", fmt_host(sa), code);    }
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

/// 弹窗正文。内容与终端输出一致，但更紧凑——弹窗放不下太多行。
///
/// 跨网地址最多列 3 条：多数机器有一堆虚拟网卡（Tailscale、Docker、
/// 各种 utun），全列出来会把窗口撑得很长，反而让人找不到重点。
fn dialog_body(code: &PairingCode, sync_port: u16, announcing: bool) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = write!(s, "配对码：{code}\n（已复制到剪贴板）\n\n");

    if announcing {
        let _ = write!(s, "在另一台设备上运行，无需输入 IP：\n    clipsync pair {code}\n");
    }

    let addrs = clipsync_net::local::local_candidates(sync_port);
    if !addrs.is_empty() {
        let _ = write!(s, "\n若不在同一局域网，改用：\n");
        for sa in addrs.iter().take(3) {
            let _ = writeln!(s, "    clipsync pair {} {}", fmt_host(sa), code);
        }
        if addrs.len() > 3 {
            let _ = writeln!(s, "    （另有 {} 个地址，见终端输出）", addrs.len() - 3);
        }
    }

    let _ = write!(s, "\n正在等待对方连接…");
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
    let code =
        PairingCode::parse(code_str).with_context(|| format!("配对码格式非法: {code_str}"))?;

    let addr = match host_ip {
        Some(ip) => format!("{ip}:{PAIRING_PORT}"),
        None => discover_host()?,
    };

    println!("  正在连接 {addr} …");
    let mut stream = TcpStream::connect(&addr).with_context(|| format!("连接 {addr} 失败"))?;
    println!("  已连接，正在协商…");

    let local = local_info(identity, device_name, sync_port);
    let record = run_pairing(&mut stream, &code, &local).context("配对握手失败")?;

    config::upsert_pairing(dir, record.clone())?;
    println!("  ✓ 配对成功：{} ({})", record.name, record.device);
    print_learned_addrs(&record);
    Ok(record)
}

/// 在局域网中查找正在等待配对的设备，返回其地址。
fn discover_host() -> Result<String> {
    use clipsync_net::discovery::discover_pairing_hosts;

    println!("  正在局域网中查找等待配对的设备…");
    let hosts = discover_pairing_hosts(std::time::Duration::from_secs(6))
        .context("局域网配对发现失败")?;

    match hosts.len() {
        0 => anyhow::bail!(
            "未在局域网中找到等待配对的设备。\n  \
             请确认对方已运行 `clipsync pair --host`；\n  \
             若两台设备不在同一局域网，请改用：clipsync pair <对方IP> <配对码>"
        ),
        1 => {
            let h = &hosts[0];
            println!("  找到设备：{}（{}）", h.device_name, h.addr.ip());
            Ok(h.addr.to_string())
        }
        _ => {
            // 多台同时在等待配对：让用户明确指定，避免连错设备。
            let mut msg = String::from("局域网中有多台设备在等待配对，请指定其一：\n");
            for h in &hosts {
                msg.push_str(&format!(
                    "      clipsync pair {} <配对码>   # {}\n",
                    h.addr.ip(),
                    h.device_name
                ));
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
    println!("  已记录对方 {} 个可达地址：", record.addrs.len());
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
mod tests {
    use super::*;

    /// 回归：主持会话到期后必须**释放端口**。
    ///
    /// 这是用户实际遇到的那个故障的核心：原实现 `loop { accept() }` 无超时
    /// 无取消，用户关掉配对码窗口后线程永远卡在 accept 上，listener 永不析构，
    /// 47685 被占死，再点菜单就是 `os error 10048`（Windows）/ `48`（macOS），
    /// 只能重启程序。
    ///
    /// 这里不走完整的 `host()`（它会生成配对码、起组播、等 3 分钟），只针对
    /// 根因——`accept_until` 到期返回、listener 随之析构——做验证。
    #[test]
    fn accept_until_releases_port_after_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let deadline = Instant::now() + Duration::from_millis(300);
        let got = accept_until(&listener, deadline).expect("轮询不应报错");
        assert!(got.is_none(), "无人连入时应到期返回 None，而不是永久阻塞");
        assert!(Instant::now() >= deadline, "应确实等到了截止时间");

        // 关键：会话结束后端口必须能被重新绑定。
        drop(listener);
        TcpListener::bind(("127.0.0.1", port))
            .expect("会话结束后端口应已释放——绑不上就意味着 10048 那个故障还在");
    }

    /// 到期前有人连入时应正常返回连接，且流已恢复为阻塞模式。
    ///
    /// 非阻塞 listener 接出来的流默认也是非阻塞的，直接拿去跑同步握手会
    /// 一路撞 `WouldBlock`——这条守的就是那个转换没被漏掉。
    #[test]
    fn accept_until_returns_connection_in_blocking_mode() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();

        // 客户端**延迟**再发数据：服务端必然先进入 read。
        // 阻塞流会一直等到数据到达；非阻塞流则立刻 WouldBlock 报错。
        // （不能用"读超时是否返回 WouldBlock"来区分——超时在 macOS 上返回的
        //  同样是 WouldBlock，两种情况分不开。）
        let client = std::thread::spawn(move || {
            use std::io::Write;
            let mut s = TcpStream::connect(addr).unwrap();
            std::thread::sleep(Duration::from_millis(150));
            s.write_all(b"X").unwrap();
            s
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let (stream, _) = accept_until(&listener, deadline)
            .expect("不应报错")
            .expect("应接到连接");

        let mut buf = [0u8; 1];
        std::io::Read::read_exact(&mut &stream, &mut buf)
            .expect("流仍是非阻塞模式——同步握手会一路撞 WouldBlock");
        assert_eq!(&buf, b"X");

        let _ = client.join();
    }

    /// 手动目视验证：弹出与托盘「显示配对码」**完全一致**的窗口。
    ///
    /// 这里刻意复用真实的 `dialog_body` 与 `show_info_and_copy`，而不是另写
    /// 一段相似的内容——照抄一遍只能证明抄得对，证明不了线上那条路径对。
    /// 唯一省略的是 `TcpListener` 与 `accept` 循环：它们与"窗口显示成什么样"
    /// 无关，却会让测试永久阻塞。
    ///
    /// 跑法：`cargo test -p clipsync-app --bin clipsync -- --ignored pairing_dialog`
    ///
    /// 判据：窗口标题为「ClipSync 配对」，正文首行的配对码可读，
    /// 且该配对码已进入剪贴板（正文里"已复制到剪贴板"这句得是真的）。
    #[test]
    #[ignore = "会弹窗并阻塞，需人工/脚本关闭"]
    fn manual_pairing_dialog() {
        let code = PairingCode::generate();
        // 打到 stdout，供外部脚本比对窗口里显示的是不是同一个码。
        println!("EXPECT_CODE={code}");
        crate::dialog::show_info_and_copy(
            "ClipSync 配对",
            &dialog_body(&code, 47_684, true),
            &code.to_string(),
        );
    }
}
