//! ClipSync 应用入口（M3：自动发现与路径优选）。
//!
//! 子命令：
//!   - `clipsync`（无参数）：后台同步——监听入站、按地址簿优先级拨号对端、
//!     局域网信标发现、剪贴板双向同步。
//!   - `clipsync pair --host`：主持配对，显示配对码等待对方连入。
//!   - `clipsync pair <ip> <code>`：连接对方完成配对。
//!   - `clipsync list`：列出已配对设备。
//!   - `clipsync addrs`：显示本机可达地址及其分类（排查连通性用）。
//!
//! **跨网络通用性**：不针对任何组网产品做适配。对端地址来自三个通用来源——
//! 配对时交换、局域网组播信标、已连接对端经加密通道通告——再按"同网段直连 →
//! 覆盖网/VPN → 公网"自动优选。因此 Tailscale、ZeroTier、Netbird、WireGuard
//! 乃至公网端口转发都能直接工作。

mod addrbook;
mod autostart;
mod compress;
mod config;
mod filecache;
mod filetransfer;
mod hub;
mod net_manager;
mod pairing_cli;
mod ratelimit;
mod tray;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clipsync_clip::{ArboardClipboard, Clipboard, ClipboardWatcher, PollingWatcher};
use clipsync_core::SyncEngine;
use clipsync_net::peer::AddrSource;
use tracing::{info, warn};

fn init_logging() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("CLIPSYNC_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

fn main() -> Result<()> {
    init_logging();

    let dir = config::config_dir()?;
    let identity = config::load_or_init_identity(&dir)?;
    let device_name = hostname_best_effort();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("pair") => match args.get(1).map(|s| s.as_str()) {
            Some("--host") | Some("-h") => {
                let settings = config::load_or_init_settings(&dir)?;
                pairing_cli::host(&dir, &identity, &device_name, settings.listen_port)
            }
            Some(first) => {
                let settings = config::load_or_init_settings(&dir)?;
                // 两种用法：
                //   clipsync pair <配对码>          局域网自动发现对方
                //   clipsync pair <对方IP> <配对码>  跨网络时显式指定
                // 靠"能否解析为合法配对码"区分——配对码字符集不含点与冒号，
                // 与 IP 地址天然不会混淆。
                if clipsync_net::pairing::PairingCode::parse(first).is_some()
                    && args.get(2).is_none()
                {
                    pairing_cli::join(&dir, &identity, &device_name, None, first, settings.listen_port)
                } else {
                    let code = args.get(2).map(|s| s.as_str()).unwrap_or("");
                    pairing_cli::join(
                        &dir,
                        &identity,
                        &device_name,
                        Some(first),
                        code,
                        settings.listen_port,
                    )
                }
            }
            None => {
                eprintln!("用法:");
                eprintln!("  clipsync pair --host             主持配对，显示配对码");
                eprintln!("  clipsync pair <配对码>            局域网内自动找到对方");
                eprintln!("  clipsync pair <对方IP> <配对码>   跨网络时显式指定");
                Ok(())
            }
        },
        Some("list") => {
            let pairings = config::load_pairings(&dir)?;
            if pairings.is_empty() {
                println!("尚无已配对设备。用 `clipsync pair --host` 开始配对。");
            } else {
                println!("已配对设备（{}）：", pairings.len());
                for p in pairings {
                    println!("  - {} ({})", p.name, p.device);
                    for a in &p.addrs {
                        println!("      {a}");
                    }
                }
            }
            Ok(())
        }
        Some("addrs") => {
            let settings = config::load_or_init_settings(&dir)?;
            print_local_addrs(settings.listen_port);
            Ok(())
        }
        Some("autostart") => {
            match args.get(1).map(|s| s.as_str()) {
                Some("on") => {
                    autostart::set_enabled(true)?;
                    println!("已开启开机自启。");
                }
                Some("off") => {
                    autostart::set_enabled(false)?;
                    println!("已关闭开机自启。");
                }
                _ => {
                    println!(
                        "开机自启：{}",
                        if autostart::is_enabled() {
                            "已开启"
                        } else {
                            "未开启"
                        }
                    );
                    println!("用法: clipsync autostart [on|off]");
                }
            }
            Ok(())
        }
        Some(other) => {
            eprintln!("未知命令: {other}");
            eprintln!("用法: clipsync [pair|list|addrs|autostart]");
            Ok(())
        }
        None => run_sync(dir, identity, device_name),
    }
}

/// 打印本机可达地址及其性质，便于排查连通性。
///
/// 注意：这里描述的是地址**性质**而非优先级。优先级由对端按自身网络位置判定
/// ——同一个 192.168.x 地址，对同网段设备是"直连"，对异地设备则不可达；
/// 覆盖网地址则相反。故此处只如实说明每个地址的类型。
fn print_local_addrs(port: u16) {
    use clipsync_net::local::{local_candidates, local_networks};

    let cands = local_candidates(port);
    println!("本机可达地址（配对与连接时会告知对端）：");
    if cands.is_empty() {
        println!("  （未找到可用地址，请检查网络连接）");
        return;
    }
    for sa in &cands {
        println!("  {:<42} {}", sa.to_string(), describe_addr(sa.ip()));
    }

    let nets = local_networks();
    println!();
    println!(
        "本机网段：IPv4 {} 个、IPv6 {} 个",
        nets.v4.len(),
        nets.v6.len()
    );
    println!();
    println!("说明：覆盖网（Tailscale / ZeroTier / Netbird / WireGuard 等）的虚拟网卡地址");
    println!("      会自动出现在上表中，无需任何额外配置。对端连接时按");
    println!("      「同网段直连 → 覆盖网 → 公网」的顺序自动优选最快路径。");
}

/// 用平实语言描述一个地址的性质。
fn describe_addr(ip: std::net::IpAddr) -> &'static str {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            if o[0] == 100 && (64..128).contains(&o[1]) {
                "覆盖网段 (100.64/10 CGNAT，Tailscale 等常用)"
            } else if v4.is_private() {
                "局域网/私有网段"
            } else {
                "公网地址"
            }
        }
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments();
            if (seg[0] & 0xfe00) == 0xfc00 {
                "覆盖网段 (IPv6 ULA)"
            } else {
                "公网 IPv6"
            }
        }
    }
}

/// 后台同步主流程。
fn run_sync(
    dir: std::path::PathBuf,
    identity: clipsync_net::crypto::StaticIdentity,
    device_name: String,
) -> Result<()> {
    let settings = config::load_or_init_settings(&dir)?;
    let device_id = identity.device_id();
    info!("配置目录: {}", dir.display());
    info!("本机设备: {} ({})", device_name, device_id);
    info!(
        "设置: 上限={} MiB, 图片={}, 文件={}, 同步端口={}",
        settings.max_bytes / (1024 * 1024),
        settings.allow_image,
        settings.allow_files,
        settings.listen_port,
    );

    let pairings = config::load_pairings(&dir)?;
    if pairings.is_empty() {
        warn!("尚无已配对设备。请先运行 `clipsync pair --host` 配对，否则不会同步。");
    } else {
        info!("已配对设备 {} 台", pairings.len());
    }

    // 地址簿：汇聚配对交换、局域网信标、对端通告三个来源。
    let addrbook = addrbook::AddrBook::new();
    net_manager::seed_addrbook_from_pairings(&addrbook, &pairings);
    seed_manual_addrs(&addrbook, &pairings);

    // 文件内容缓存（断点续传 + 重复内容秒同步）与待发文件登记表。
    let cache = Arc::new(filecache::FileCache::new(settings.file_cache_bytes)?);
    let received_dir = cache.received_dir();
    let outgoing = filetransfer::OutgoingFiles::new();
    info!(
        "文件缓存: {}（上限 {} MiB）",
        received_dir.display(),
        settings.file_cache_bytes / (1024 * 1024)
    );

    // 中枢：唯一持有引擎，串行处理本地/远端事件。
    let engine = SyncEngine::new(device_id.clone(), settings.to_limits());
    let clipboard = Arc::new(Mutex::new(ArboardClipboard::new()?));
    // 托盘状态：中枢更新连接数，托盘读取展示；暂停标志双方共享。
    let status = tray::TrayStatus::new(pairings.len());
    let (hub, _hub_thread) = hub::start_hub(
        engine,
        hub::HubDeps {
            clipboard,
            addrbook: addrbook.clone(),
            cache: cache.clone(),
            outgoing: outgoing.clone(),
            status: status.clone(),
        },
    );

    // 网络上下文。
    let identity_for_tray = identity.clone();
    let ctx = net_manager::NetCtx {
        local_device: device_id.clone(),
        identity: Arc::new(identity),
        known: Arc::new(pairings.iter().cloned().map(Into::into).collect()),
        hub: hub.clone(),
        registry: net_manager::ConnRegistry::new(),
        addrbook: addrbook.clone(),
        outgoing,
        upload_limit: Some(settings.upload_limit_bytes_per_sec),
        compress_transfers: settings.compress_transfers,
        sync_port: settings.listen_port,
    };
    net_manager::spawn_listener(ctx.clone())?;
    net_manager::spawn_dialer(ctx.clone());

    start_discovery(&device_id, &addrbook, &pairings, settings.listen_port);

    // 剪贴板监听线程：本地变化 → 中枢。
    // 设 CLIPSYNC_NO_WATCH=1 可禁用（纯接收设备，或用于测试隔离）。
    let running = Arc::new(AtomicBool::new(true));
    if std::env::var_os("CLIPSYNC_NO_WATCH").is_none() {
        spawn_clip_watch(hub.clone(), running.clone(), received_dir)?;
    } else {
        info!("已禁用本地剪贴板监听（CLIPSYNC_NO_WATCH），仅接收远端内容。");
    }

    let r = running.clone();
    ctrlc::set_handler(move || r.store(false, Ordering::SeqCst)).expect("设置 Ctrl+C 处理器失败");

    info!("ClipSync 已启动。复制内容将同步到已配对且在线的设备。");

    // 无托盘模式：用于服务器/测试环境（或托盘创建失败时的兜底）。
    if std::env::var_os("CLIPSYNC_NO_TRAY").is_some() {
        info!("已禁用托盘（CLIPSYNC_NO_TRAY）。按 Ctrl+C 退出。");
        while running.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        info!("正在退出…");
        return Ok(());
    }

    // 托盘必须在主线程运行；同步逻辑已全部在后台线程中。
    run_tray(status, running, dir, identity_for_tray, device_name, settings.listen_port)
}

/// 在主线程运行托盘，处理菜单动作直到用户退出。
fn run_tray(
    status: tray::TrayStatus,
    running: Arc<AtomicBool>,
    dir: std::path::PathBuf,
    identity: clipsync_net::crypto::StaticIdentity,
    device_name: String,
    sync_port: u16,
) -> Result<()> {
    let status_for_cb = status.clone();
    let running_for_cb = running.clone();

    let callbacks = tray::TrayCallbacks {
        on_action: Box::new(move |action| match action {
            tray::TrayAction::TogglePause => {
                let now = !status_for_cb.is_paused();
                status_for_cb.set_paused(now);
                info!("同步已{}", if now { "暂停" } else { "恢复" });
                true
            }
            tray::TrayAction::ToggleAutostart => {
                let now = !autostart::is_enabled();
                match autostart::set_enabled(now) {
                    Ok(()) => info!("开机自启已{}", if now { "开启" } else { "关闭" }),
                    Err(e) => warn!("设置开机自启失败: {e:#}"),
                }
                true
            }
            tray::TrayAction::ShowPairingCode => {
                // 在后台线程主持配对，避免阻塞托盘事件循环。
                let dir = dir.clone();
                let identity = identity.clone();
                let name = device_name.clone();
                std::thread::spawn(move || {
                    if let Err(e) = pairing_cli::host(&dir, &identity, &name, sync_port) {
                        warn!("配对失败: {e:#}");
                    }
                });
                true
            }
            tray::TrayAction::Quit => {
                info!("正在退出…");
                running_for_cb.store(false, Ordering::SeqCst);
                false
            }
        }),
    };

    if let Err(e) = tray::run(status, callbacks) {
        // 托盘不可用（无桌面会话等）时退化为纯后台运行，不影响同步。
        warn!("托盘不可用，转为纯后台运行: {e:#}");
        while running.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    Ok(())
}

/// 启动局域网信标：宣告本机 + 发现同网段的已配对设备。
fn start_discovery(
    device_id: &clipsync_core::DeviceId,
    addrbook: &addrbook::AddrBook,
    pairings: &[clipsync_net::pairing::PairingRecord],
    sync_port: u16,
) {
    use clipsync_net::discovery;

    // 发送：周期性宣告本机身份与全部可达地址。
    let sender_port = sync_port;
    if let Err(e) = discovery::spawn_sender(device_id.clone(), sync_port, move || {
        clipsync_net::local::local_candidates(sender_port)
    }) {
        warn!("启动信标发送失败（局域网发现不可用）: {e:#}");
    }

    // 接收：只接纳已配对设备的信标，其余忽略。
    let known: std::collections::HashSet<clipsync_core::DeviceId> =
        pairings.iter().map(|p| p.device.clone()).collect();
    let book = addrbook.clone();
    match discovery::spawn_listener(device_id.clone(), move |found| {
        if !known.contains(&found.device) {
            return; // 非已配对设备，忽略
        }
        // 信标源 IP 是最可靠的局域网地址；同时收下对端宣告的其它地址。
        book.add_addrs(&found.device, [found.lan_addr], AddrSource::Beacon);
        book.add_addrs(&found.device, found.extra_addrs, AddrSource::Beacon);
    }) {
        Ok(_) => {}
        Err(e) => warn!(
            "启动信标监听失败（同机多实例时属正常，将依赖配对/通告地址）: {e:#}"
        ),
    }
}

/// 把 `CLIPSYNC_PEERS` 指定的地址作为手动来源加入地址簿。
///
/// 用于自动发现无法覆盖的场景（如公网端口转发、固定 DDNS）。格式 `ip:port`，
/// 逗号分隔；会为每个已配对设备都加入这些候选（握手时的公钥认证会拒绝错配）。
fn seed_manual_addrs(
    addrbook: &addrbook::AddrBook,
    pairings: &[clipsync_net::pairing::PairingRecord],
) {
    let raw = match std::env::var("CLIPSYNC_PEERS") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return,
    };
    let addrs: Vec<std::net::SocketAddr> = raw
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    if addrs.is_empty() {
        return;
    }
    for p in pairings {
        addrbook.add_addrs(&p.device, addrs.iter().copied(), AddrSource::Manual);
    }
    info!("已加入 {} 个手动指定地址（CLIPSYNC_PEERS）", addrs.len());
}

/// 启动剪贴板监听线程：本地变化 → 读取 → 投递给中枢。
///
/// `received_dir` 是本程序落地接收文件的目录。若一次剪贴板变化的文件**全部**
/// 位于其中，说明这是我们自己刚写入剪贴板的接收结果，不应再广播回去——否则
/// 两台设备会来回推送同一批文件形成回环。
///
/// 文本与图片的回环由引擎的哈希登记（`expect_echo`）拦截；文件走这条路径是
/// 因为落地后的文件修改时间与源文件不同，标识必然改变，无法用哈希比对识别。
fn spawn_clip_watch(
    hub: hub::HubHandle,
    running: Arc<AtomicBool>,
    received_dir: std::path::PathBuf,
) -> Result<()> {
    let mut reader = ArboardClipboard::new()?;
    let mut watcher = PollingWatcher::new()?;
    std::thread::Builder::new()
        .name("clip-watch".into())
        .spawn(move || {
            let r = watcher.run(&mut || {
                if !running.load(Ordering::SeqCst) {
                    return false;
                }
                match reader.read() {
                    Ok(Some(read)) => {
                        if is_our_received_files(&read, &received_dir) {
                            tracing::debug!("跳过本机刚落地的接收文件（避免回环）");
                            return true;
                        }
                        hub.send(hub::HubEvent::Local {
                            content: read.content,
                            sensitive: read.sensitive,
                            file_paths: read.file_paths,
                        });
                    }
                    Ok(None) => {}
                    Err(e) => warn!("读取剪贴板失败: {e:#}"),
                }
                true
            });
            if let Err(e) = r {
                warn!("剪贴板监听线程退出: {e:#}");
            }
        })?;
    Ok(())
}

/// 这次剪贴板内容是否为本程序自己落地的接收文件。
fn is_our_received_files(read: &clipsync_clip::ClipRead, received_dir: &std::path::Path) -> bool {
    !read.file_paths.is_empty()
        && read
            .file_paths
            .iter()
            .all(|p| p.starts_with(received_dir))
}

/// 尽力获取主机名（跨平台，不引额外依赖）。
fn hostname_best_effort() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-host".to_string())
}
