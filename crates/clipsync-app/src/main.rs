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

// Windows 上编译为 GUI 子系统：双击 exe 或开机自启时不再弹出一个空的黑色
// 控制台窗口——那对一个"安静常驻"的托盘程序是明显的打扰。
//
// **命令行子命令不受影响**：`windows_subsystem` 只决定"要不要自动分配一个
// 新控制台"。从已有终端（cmd/PowerShell）运行时，进程照样继承父进程的控制台，
// `println!` 正常可见。真正会丢输出的是双击运行——而那种情况本来就没人看
// 终端，配对码等关键信息一律另走弹窗（见 `dialog` 模块）。
//
// debug 构建保留控制台，方便开发期直接看日志。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod addrbook;
mod autostart;
mod cli;
mod compress;
mod config;
mod device_name;
mod dialog;
mod filecache;
mod filetransfer;
mod hub;
mod known_peers;
mod logging;
mod net_manager;
mod pairing_cli;
mod pairing_ui;
mod ratelimit;
mod size_parse;
mod tray;
mod tray_bridge;
mod wiring;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clipsync_clip::ArboardClipboard;
use clipsync_core::SyncEngine;
use tracing::{info, warn};

fn main() -> Result<()> {
    // 日志要先于一切初始化，但它需要配置目录（日志写在那儿），而定位配置
    // 目录本身也可能出错——那一步的错误只能走 stderr，此时还没有日志设施。
    let dir = config::config_dir()?;
    // 读设置只为拿 verbose 开关；失败就按默认（不详细）来，不能因为设置文件
    // 有问题就连日志都不初始化——那正是最需要日志的时候。
    let verbose = config::load_or_init_settings(&dir)
        .map(|s| s.verbose_log)
        .unwrap_or(false);
    let log_control = logging::init(&dir, verbose);
    let identity = config::load_or_init_identity(&dir)?;
    let device_name = device_name::device_name_best_effort();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("pair") => match args.get(1).map(|s| s.as_str()) {
            Some("--host") | Some("-h") => {
                let settings = config::load_or_init_settings(&dir)?;
                // 命令行下终端可见，不弹窗打断。
                // 命令行下终端可见，不弹窗打断；也无需单例槽位（进程本身
                // 就是一次性的）。
                pairing_cli::host(
                    &dir,
                    &identity,
                    &device_name,
                    settings.listen_port,
                    false,
                    |_| {},
                )
                .map(|_| ())
            }
            Some(first) => {
                let settings = config::load_or_init_settings(&dir)?;
                // 三种写法，靠能否解析为配对码来区分——配对码字符集不含点、
                // 冒号与 @，与 IP 地址天然不会混淆：
                //   clipsync pair <码>           局域网自动发现
                //   clipsync pair <码>@<地址>     跨网络，一串搞定
                //   clipsync pair <地址> <码>     旧写法，仍然支持
                if let Some((code, host)) = pairing_cli::parse_pairing_input(first) {
                    pairing_cli::join(
                        &dir,
                        &identity,
                        &device_name,
                        host.as_deref(),
                        &code.to_string(),
                        settings.listen_port,
                    )
                    .map(|_| ())
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
                    .map(|_| ())
                }
            }
            None => {
                eprintln!("用法:");
                eprintln!("  clipsync pair --host             主持配对，显示配对码");
                eprintln!("  clipsync pair <配对码>            局域网内自动找到对方");
                eprintln!("  clipsync pair <配对码>@<对方IP>   跨网络（如 Tailscale）");
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
            cli::print_local_addrs(settings.listen_port);
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
        None => run_sync(dir, identity, device_name, log_control),
    }
}

/// 后台同步主流程。
fn run_sync(
    dir: std::path::PathBuf,
    identity: clipsync_net::crypto::StaticIdentity,
    device_name: String,
    log_control: logging::LogControl,
) -> Result<()> {
    let settings = config::load_or_init_settings(&dir)?;
    let device_id = identity.device_id();
    info!("配置目录: {}", dir.display());
    info!("日志文件: {}", log_control.dir().join("clipsync.log").display());
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
    wiring::seed_manual_addrs(&addrbook, &pairings);

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
    // 设置句柄：托盘改动后中枢立即读到新值，无需重启。
    let settings_handle = config::SettingsHandle::new(dir.clone(), settings.clone());
    let (hub, hub_thread) = hub::start_hub(
        engine,
        hub::HubDeps {
            clipboard,
            addrbook: addrbook.clone(),
            cache: cache.clone(),
            outgoing: outgoing.clone(),
            status: status.clone(),
            settings: settings_handle.clone(),
        },
    );

    // 已配对设备表。运行期可增补——从托盘完成配对后立即生效，无需重启。
    let known = known_peers::KnownPeers::new(pairings.iter().cloned().map(Into::into).collect());

    // 网络上下文。
    let identity_for_tray = identity.clone();
    let ctx = net_manager::NetCtx {
        local_device: device_id.clone(),
        identity: Arc::new(identity),
        known: known.clone(),
        hub: hub.clone(),
        registry: net_manager::ConnRegistry::new(),
        addrbook: addrbook.clone(),
        outgoing,
        settings: settings_handle.clone(),
        sync_port: settings.listen_port,
        config_dir: dir.clone(),
    };
    net_manager::spawn_listener(ctx.clone())?;
    net_manager::spawn_dialer(ctx.clone());

    wiring::start_discovery(&device_id, &addrbook, &known, settings.listen_port);

    // 剪贴板监听线程：本地变化 → 中枢。
    // 设 CLIPSYNC_NO_WATCH=1 可禁用（纯接收设备，或用于测试隔离）。
    let running = Arc::new(AtomicBool::new(true));
    if std::env::var_os("CLIPSYNC_NO_WATCH").is_none() {
        wiring::spawn_clip_watch(hub.clone(), running.clone(), received_dir)?;
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
    let pairing = pairing_ui::PairingDeps {
        known,
        addrbook,
        status: status.clone(),
        host_slot: pairing_ui::PairingHostSlot::default(),
        hub: hub.clone(),
        dir: dir.clone(),
    };
    tray_bridge::run_tray(
        status,
        running,
        dir,
        identity_for_tray,
        device_name,
        settings.listen_port,
        settings_handle,
        pairing,
        log_control,
        hub_thread,
    )
}
