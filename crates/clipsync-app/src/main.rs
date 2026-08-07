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
mod dialog;
mod filecache;
mod filetransfer;
mod hub;
mod logging;
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
    let device_name = device_name_best_effort();

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
                // 两种用法：
                //   clipsync pair <配对码>          局域网自动发现对方
                //   clipsync pair <对方IP> <配对码>  跨网络时显式指定
                // 靠"能否解析为合法配对码"区分——配对码字符集不含点与冒号，
                // 与 IP 地址天然不会混淆。
                if clipsync_net::pairing::PairingCode::parse(first).is_some()
                    && args.get(2).is_none()
                {
                    pairing_cli::join(&dir, &identity, &device_name, None, first, settings.listen_port)
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
        None => run_sync(dir, identity, device_name, log_control),
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
    let known = net_manager::KnownPeers::new(pairings.iter().cloned().map(Into::into).collect());

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
    };
    net_manager::spawn_listener(ctx.clone())?;
    net_manager::spawn_dialer(ctx.clone());

    start_discovery(&device_id, &addrbook, &known, settings.listen_port);

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
    let pairing = PairingDeps {
        known,
        addrbook,
        status: status.clone(),
        host_slot: PairingHostSlot::default(),
        hub: hub.clone(),
        dir: dir.clone(),
    };
    run_tray(
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

/// 保证同一时刻只有一个"主持配对"会话在跑。
///
/// 没有这层控制时，用户第二次点「显示配对码」会新起一个线程去 bind 已被占用
/// 的 47685，直接抛出 `os error 10048`（Windows）/ `48`（macOS）给用户看。
/// 而用户的真实意图通常只是**再看一眼那个码**——所以这里不报错、也不新开
/// 会话，而是把当前会话的配对码重新弹出来。
#[derive(Clone, Default)]
struct PairingHostSlot {
    /// 会话进行中时持有当前配对码；结束后自动清空。
    active: Arc<Mutex<Option<String>>>,
}

impl PairingHostSlot {
    /// 尝试占用槽位。已被占用时返回当前会话的配对码。
    fn try_acquire(&self) -> Result<PairingHostGuard, String> {
        let mut g = self.active.lock().unwrap();
        match g.as_ref() {
            Some(code) => Err(code.clone()),
            None => {
                *g = Some(String::new()); // 先占位，拿到码后再补
                Ok(PairingHostGuard {
                    slot: self.clone(),
                })
            }
        }
    }

    fn set_code(&self, code: &str) {
        *self.active.lock().unwrap() = Some(code.to_string());
    }

    fn release(&self) {
        *self.active.lock().unwrap() = None;
    }
}

/// 持有期间槽位被占用；**丢弃即释放**，包括 host() 提前返回错误的路径。
struct PairingHostGuard {
    slot: PairingHostSlot,
}

impl Drop for PairingHostGuard {
    fn drop(&mut self) {
        self.slot.release();
    }
}

/// 从托盘发起配对时，"配对成功"之后还需要让它**立刻**生效所需的东西。
#[derive(Clone)]
struct PairingDeps {
    known: net_manager::KnownPeers,
    addrbook: addrbook::AddrBook,
    status: tray::TrayStatus,
    /// 主持会话的单例槽位，避免重复点击撞上端口占用。
    host_slot: PairingHostSlot,
    /// 中枢句柄：解除配对时通知它断开对应连接。
    hub: hub::HubHandle,
    /// 配置目录：解除配对要落盘。
    dir: std::path::PathBuf,
}

impl PairingDeps {
    /// 登记一台刚配对成功的设备。
    ///
    /// 两处都要更新，缺一台设备就连不上：
    ///   - **设备表**——拨号线程据此决定拨谁，入站握手据此认证对端；
    ///   - **地址簿**——配对时对方告知的可达地址，是首次连接的唯一线索
    ///     （信标只覆盖同网段）。
    ///
    /// 顺带刷新托盘上的已配对台数，否则菜单首行会停在"尚未配对设备"，
    /// 而同步其实已经跑起来了。
    fn register(&self, record: &clipsync_net::pairing::PairingRecord) {
        self.known.upsert(record.clone().into());
        if !record.addrs.is_empty() {
            self.addrbook
                .add_addrs(&record.device, record.addrs.iter().copied(), AddrSource::Pairing);
        }
        self.status.set_paired(self.known.len());
        info!(
            "已登记新配对设备 {} ({})，无需重启即可开始同步",
            record.name, record.device
        );
    }

    /// 当前已配对设备及其在线状态，供托盘渲染设备列表。
    fn peers_for_tray(&self) -> Vec<tray::TrayPeer> {
        let connected = self.status.connected_devices();
        self.known
            .snapshot()
            .into_iter()
            .map(|p| tray::TrayPeer {
                online: connected.contains(p.device.as_str()),
                device: p.device.to_string(),
                name: p.name,
            })
            .collect()
    }

    /// 解除与某台设备的配对。
    ///
    /// 四处都要清，漏一处就会留下"解除了但还在连"或"列表里没了却仍被信标
    /// 接纳"这类半吊子状态：
    ///   - **磁盘记录**：否则重启后它又回来了；
    ///   - **设备表**：拨号线程与入站认证都查它，清掉才算真的断绝关系；
    ///   - **地址簿**：留着会让诊断输出显示一台已解除的设备；
    ///   - **中枢**：丢掉发送通道，当场切断已建立的连接。
    fn unpair(&self, device_id: &str) -> Result<Option<String>> {
        let device = clipsync_core::DeviceId::from_hex(device_id);
        let name = config::remove_pairing(&self.dir, &device)?;
        if name.is_none() {
            return Ok(None); // 已经不在了，无需再做
        }
        self.known.remove(&device);
        self.addrbook.forget(&device);
        self.hub.send(hub::HubEvent::Unpaired {
            device: device.clone(),
        });
        self.status.set_paired(self.known.len());
        Ok(name)
    }
}

/// 在主线程运行托盘，处理菜单动作直到用户退出。
fn run_tray(
    status: tray::TrayStatus,
    running: Arc<AtomicBool>,
    dir: std::path::PathBuf,
    identity: clipsync_net::crypto::StaticIdentity,
    device_name: String,
    sync_port: u16,
    settings: config::SettingsHandle,
    pairing: PairingDeps,
    log_control: logging::LogControl,
    hub_thread: std::thread::JoinHandle<()>,
) -> Result<()> {
    let pairing_for_peers = pairing.clone();
    let status_for_cb = status.clone();
    let running_for_cb = running.clone();
    let settings_for_cb = settings.clone();
    let settings_for_read = settings.clone();

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
                let pairing = pairing.clone();
                std::thread::spawn(move || {
                    host_pairing_interactive(&dir, &identity, &name, sync_port, &pairing);
                });
                true
            }
            tray::TrayAction::EnterPairingCode => {
                let dir = dir.clone();
                let identity = identity.clone();
                let name = device_name.clone();
                let pairing = pairing.clone();
                std::thread::spawn(move || {
                    join_by_code_interactive(&dir, &identity, &name, sync_port, &pairing);
                });
                true
            }
            tray::TrayAction::Quit => {
                info!("正在退出…");
                running_for_cb.store(false, Ordering::SeqCst);
                false
            }
            tray::TrayAction::ToggleSendImages => {
                let now = !settings_for_cb.snapshot().allow_image;
                settings_for_cb.update(|s| s.allow_image = now);
                info!("发送图片已{}", if now { "开启" } else { "关闭" });
                true
            }
            tray::TrayAction::ToggleSendFiles => {
                let now = !settings_for_cb.snapshot().allow_files;
                settings_for_cb.update(|s| s.allow_files = now);
                info!("发送文件已{}", if now { "开启" } else { "关闭" });
                true
            }
            tray::TrayAction::ToggleCompress => {
                let now = !settings_for_cb.snapshot().compress_transfers;
                settings_for_cb.update(|s| s.compress_transfers = now);
                info!("传输前自动压缩已{}", if now { "开启" } else { "关闭" });
                true
            }
            tray::TrayAction::SetMaxBytes(v) => {
                settings_for_cb.update(|s| s.max_bytes = v);
                if v == usize::MAX {
                    info!("单次大小上限：不限制");
                } else {
                    info!("单次大小上限：{} MiB", v / (1024 * 1024));
                }
                true
            }
            tray::TrayAction::SetUploadLimit(v) => {
                settings_for_cb.update(|s| s.upload_limit_bytes_per_sec = v);
                if v == 0 {
                    info!("发送限速：不限速");
                } else {
                    info!("发送限速：{} MB/s", v / 1_000_000);
                }
                true
            }
            // 三个「自定义…」都要弹框，而弹框会阻塞到用户点掉——放在托盘
            // 事件循环里会让整个菜单卡住，故一律丢到后台线程。
            tray::TrayAction::PromptMaxBytes => {
                let s = settings_for_cb.clone();
                std::thread::spawn(move || prompt_max_bytes(&s));
                true
            }
            tray::TrayAction::PromptUploadLimit => {
                let s = settings_for_cb.clone();
                std::thread::spawn(move || prompt_upload_limit(&s));
                true
            }
            tray::TrayAction::PromptListenPort => {
                let s = settings_for_cb.clone();
                std::thread::spawn(move || prompt_listen_port(&s));
                true
            }
            tray::TrayAction::ToggleVerboseLog => {
                let now = !settings_for_cb.snapshot().verbose_log;
                match log_control.set_verbose(now) {
                    Ok(()) => settings_for_cb.update(|s| s.verbose_log = now),
                    // 切不动就别改设置，免得界面显示"已开启"而实际没生效。
                    Err(e) => warn!("切换日志级别失败: {e:#}"),
                }
                true
            }
            tray::TrayAction::OpenLogDir => {
                if let Err(e) = logging::open_in_file_manager(log_control.dir()) {
                    warn!("打开日志文件夹失败: {e:#}");
                    crate::dialog::show_info(
                        "ClipSync 日志",
                        &format!("日志位于：\n{}\n\n（自动打开失败：{e}）", log_control.dir().display()),
                    );
                }
                true
            }
            tray::TrayAction::Unpair(device_id) => {
                // 确认框会阻塞到用户点掉，丢到后台线程免得卡住托盘。
                let pairing = pairing.clone();
                std::thread::spawn(move || unpair_interactive(&pairing, &device_id));
                true
            }
        }),
        current_settings: Box::new(move || {
            let s = settings_for_read.snapshot();
            tray::TraySettings {
                send_images: s.allow_image,
                send_files: s.allow_files,
                compress: s.compress_transfers,
                max_bytes: s.max_bytes,
                upload_limit: s.upload_limit_bytes_per_sec,
                listen_port: s.listen_port,
                verbose_log: s.verbose_log,
            }
        }),
        current_peers: Box::new(move || pairing_for_peers.peers_for_tray()),
        // 中枢线程一旦结束（正常退出或 panic），同步就全停了。托盘据此
        // 切到故障状态，而不是继续显示一切正常。
        hub_alive: Box::new(move || !hub_thread.is_finished()),
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

/// 托盘里点某台设备 → 确认 → 解除配对。
///
/// 先确认再动手：这是不可撤销的操作，解除后要重新走一遍配对流程才能恢复。
fn unpair_interactive(pairing: &PairingDeps, device_id: &str) {
    // 名字从当前设备表取，弹窗里要让用户看清解除的是哪一台。
    let name = pairing
        .known
        .snapshot()
        .into_iter()
        .find(|p| p.device.as_str() == device_id)
        .map(|p| p.name)
        .unwrap_or_else(|| device_id.to_string());

    if !dialog::confirm(
        "ClipSync 解除配对",
        &format!(
            "确定要解除与「{name}」的配对吗？\n\n\
             解除后双方将立即断开、不再同步。\n\
             要恢复需要重新走一次配对流程。"
        ),
    ) {
        return;
    }

    match pairing.unpair(device_id) {
        Ok(Some(name)) => {
            info!("已解除与 {name} 的配对");
            dialog::show_info("ClipSync", &format!("已解除与「{name}」的配对。"));
        }
        Ok(None) => info!("设备 {device_id} 已不在配对列表中，无需解除"),
        Err(e) => {
            warn!("解除配对失败: {e:#}");
            dialog::show_info("ClipSync 解除配对失败", &format!("{e:#}"));
        }
    }
}

/// 弹框自定义单次大小上限。
///
/// 输入非法时**再弹一次说明**而不是静默忽略：用户刚打完一串字，什么反馈都
/// 没有只会让人以为程序坏了。
fn prompt_max_bytes(settings: &config::SettingsHandle) {
    let current = settings.snapshot().max_bytes;
    let Some(input) = dialog::prompt(
        "ClipSync 单次大小上限",
        &format!(
            "当前：{}\n\n输入新的上限，例如 500MB、1.5GiB、200m：\n（填 0 表示不限制）",
            describe_bytes(current)
        ),
    ) else {
        return;
    };

    match config::parse_byte_size(&input) {
        Ok(0) => {
            settings.update(|s| s.max_bytes = usize::MAX);
            info!("单次大小上限：不限制");
        }
        Ok(v) => {
            let v = v as usize;
            settings.update(|s| s.max_bytes = v);
            info!("单次大小上限：{} 字节", v);
        }
        Err(e) => dialog::show_info("ClipSync 设置未生效", &format!("{e}")),
    }
}

/// 弹框自定义发送限速。
fn prompt_upload_limit(settings: &config::SettingsHandle) {
    let current = settings.snapshot().upload_limit_bytes_per_sec;
    let shown = if current == 0 {
        "不限速".to_string()
    } else {
        format!("{}/s", describe_bytes(current as usize))
    };
    let Some(input) = dialog::prompt(
        "ClipSync 发送限速",
        &format!(
            "当前：{shown}\n\n输入新的限速，例如 10MB/s、20mbps、5M：\n（填 0 表示不限速）"
        ),
    ) else {
        return;
    };

    match config::parse_rate(&input) {
        Ok(v) => {
            settings.update(|s| s.upload_limit_bytes_per_sec = v);
            if v == 0 {
                info!("发送限速：不限速");
            } else {
                info!("发送限速：{} 字节/秒", v);
            }
        }
        Err(e) => dialog::show_info("ClipSync 设置未生效", &format!("{e}")),
    }
}

/// 弹框修改同步监听端口。
///
/// 端口与其它设置不同：**改了要重启才生效**——监听套接字在启动时就绑好了，
/// 运行中换端口意味着断开所有连接重新监听，还要让对端重新学到新端口。
/// 与其做一套半可靠的热切换，不如如实告诉用户重启一下。
fn prompt_listen_port(settings: &config::SettingsHandle) {
    let current = settings.snapshot().listen_port;
    let Some(input) = dialog::prompt(
        "ClipSync 同步端口",
        &format!("当前：{current}\n\n输入新的端口（1024–65535）：\n改动将在下次启动时生效。"),
    ) else {
        return;
    };

    match input.trim().parse::<u16>() {
        // 1024 以下是特权端口，普通用户绑不上，提前拦住比让它启动时失败好。
        Ok(p) if p >= 1024 => {
            settings.update(|s| s.listen_port = p);
            info!("同步端口已改为 {p}（重启后生效）");
            dialog::show_info(
                "ClipSync 同步端口",
                &format!("已设为 {p}。\n\n请重启 ClipSync 使其生效，并确认对端也能连到这个端口。"),
            );
        }
        Ok(p) => dialog::show_info(
            "ClipSync 设置未生效",
            &format!("端口 {p} 属于系统保留范围，请填 1024–65535 之间的值。"),
        ),
        Err(_) => dialog::show_info(
            "ClipSync 设置未生效",
            &format!("「{input}」不是有效端口，请填 1024–65535 之间的整数。"),
        ),
    }
}

/// 把字节数写成人能读的形式（弹窗里展示当前值用）。
fn describe_bytes(n: usize) -> String {
    if n == usize::MAX {
        return "不限制".to_string();
    }
    if n >= 1 << 30 {
        format!("{:.2} GiB", n as f64 / (1u64 << 30) as f64)
    } else if n >= 1 << 20 {
        format!("{:.0} MiB", n as f64 / (1u64 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.0} KiB", n as f64 / 1024.0)
    } else {
        format!("{n} 字节")
    }
}

/// 托盘「显示配对码…」的完整流程，含单例控制。
///
/// 重复点击时**不再** bind 一个已被占用的端口（那会给用户抛 `os error 10048`
/// 且只能重启程序恢复），而是把当前会话的配对码重新弹出来——这本就是用户
/// 重复点击时想要的。
fn host_pairing_interactive(
    dir: &std::path::Path,
    identity: &clipsync_net::crypto::StaticIdentity,
    device_name: &str,
    sync_port: u16,
    pairing: &PairingDeps,
) {
    let guard = match pairing.host_slot.try_acquire() {
        Ok(g) => g,
        Err(code) => {
            // 已有会话在等待——把同一个码再显示一遍即可。
            info!("配对会话已在进行中，重新显示当前配对码");
            dialog::show_info_and_copy(
                "ClipSync 配对",
                &format!(
                    "配对码：{code}\n（已复制到剪贴板）\n\n\
                     本机仍在等待对方加入，请在对方设备上选「输入配对码…」。"
                ),
                &code,
            );
            return;
        }
    };

    let slot = pairing.host_slot.clone();
    let result = pairing_cli::host(dir, identity, device_name, sync_port, true, |code| {
        slot.set_code(code.as_str());
    });
    drop(guard); // 显式释放槽位，后续弹窗期间允许再次发起

    match result {
        Ok(record) => {
            pairing.register(&record);
            dialog::show_info(
                "ClipSync 配对成功",
                &format!("已与「{}」配对，现在可以互相同步了。", record.name),
            );
        }
        Err(e) => {
            warn!("配对失败: {e:#}");
            dialog::show_info("ClipSync 配对失败", &format!("{e:#}"));
        }
    }
}

/// 托盘「输入配对码…」的完整流程：要码 → 加入 → 告知结果。
///
/// 跑在后台线程（弹窗会阻塞到用户点掉，不能占着托盘事件循环）。
///
/// **地址从哪来**：先按局域网自动发现找主持方；找不到再问一次对方地址——
/// 主持方那边的窗口里就列着可用地址，照抄即可。这样同局域网的常见情形
/// 全程只需输一个配对码，跨网络也不至于卡死没有出路。
fn join_by_code_interactive(
    dir: &std::path::Path,
    identity: &clipsync_net::crypto::StaticIdentity,
    device_name: &str,
    sync_port: u16,
    pairing: &PairingDeps,
) {
    let Some(code) = dialog::prompt(
        "ClipSync 配对",
        "请输入对方显示的配对码：\n（在对方设备的托盘菜单里选「显示配对码…」）",
    ) else {
        return; // 用户取消
    };

    if clipsync_net::pairing::PairingCode::parse(&code).is_none() {
        dialog::show_info(
            "ClipSync 配对失败",
            &format!("配对码「{code}」格式不正确，请核对后重试。"),
        );
        return;
    }

    // 先试局域网自动发现。
    match pairing_cli::join(dir, identity, device_name, None, &code, sync_port) {
        Ok(record) => {
            pairing.register(&record);
            dialog::show_info(
                "ClipSync 配对成功",
                &format!("已与「{}」配对，现在可以互相同步了。", record.name),
            );
            return;
        }
        Err(e) => {
            info!("局域网自动发现未能完成配对，改为询问对方地址: {e:#}");
        }
    }

    // 自动发现走不通（不同局域网、组播被拦），退而求其次问地址。
    let Some(host) = dialog::prompt(
        "ClipSync 配对",
        "没能在局域网里找到对方。\n请输入对方设备的 IP 地址（对方窗口里有列出）：",
    ) else {
        return;
    };

    match pairing_cli::join(dir, identity, device_name, Some(&host), &code, sync_port) {
        Ok(record) => {
            pairing.register(&record);
            dialog::show_info(
                "ClipSync 配对成功",
                &format!("已与「{}」配对，现在可以互相同步了。", record.name),
            );
        }
        Err(e) => {
            warn!("配对失败: {e:#}");
            dialog::show_info("ClipSync 配对失败", &format!("{e:#}"));
        }
    }
}

/// 启动局域网信标：宣告本机 + 发现同网段的已配对设备。
fn start_discovery(
    device_id: &clipsync_core::DeviceId,
    addrbook: &addrbook::AddrBook,
    known: &net_manager::KnownPeers,
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
    //
    // 这里持有的是共享的设备表而非启动时的快照：运行期新配对的设备，其信标
    // 也应当立刻被接纳，否则新设备要等到重启才会被局域网发现。
    let known = known.clone();
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

/// 尽力获取本机设备名（跨平台，不引额外依赖）。
///
/// 这个名字会在配对时发给对端并被其持久化，是用户在 `list` 与托盘里辨认
/// "哪台机器"的唯一依据，因此不能轻易退化成占位符。
///
/// 依次尝试：
///   1. `CLIPSYNC_DEVICE_NAME` —— 用户显式指定，优先级最高；
///   2. `COMPUTERNAME` —— Windows 由系统设置，可靠；
///   3. `scutil --get ComputerName` —— macOS 上用户在"设置 › 通用 › 关于本机"
///      里看到的那个名字（可含空格与中文），比主机名更贴近用户认知；
///   4. `hostname` 命令 —— 各 Unix 通用兜底，去掉 `.local` 之类的域名后缀；
///   5. `HOSTNAME` 环境变量 —— 某些 shell 会导出。
///
/// **为什么不能只看环境变量**：`HOSTNAME` 是 bash 的 shell 变量，默认并不
/// 导出；zsh 根本不设它，从 launchd/Finder 启动更是没有。实测在 macOS 上
/// 两个变量都不存在，原实现必然退化为 `unknown-host`——两台 Mac 配对后
/// 彼此都显示同一个名字，无法区分。
fn device_name_best_effort() -> String {
    if let Some(name) = non_empty(std::env::var("CLIPSYNC_DEVICE_NAME").ok()) {
        return name;
    }
    if let Some(name) = non_empty(std::env::var("COMPUTERNAME").ok()) {
        return name;
    }

    #[cfg(target_os = "macos")]
    if let Some(name) = non_empty(run_capture("scutil", &["--get", "ComputerName"])) {
        return name;
    }

    #[cfg(unix)]
    if let Some(name) = non_empty(run_capture("hostname", &[])) {
        // `hostname` 常返回 `foo.local` / FQDN，取首段更适合展示。
        let short = name.split('.').next().unwrap_or(&name).to_string();
        if let Some(short) = non_empty(Some(short)) {
            return short;
        }
    }

    if let Some(name) = non_empty(std::env::var("HOSTNAME").ok()) {
        return name;
    }
    "unknown-host".to_string()
}

/// 去掉首尾空白；结果为空则视为"没取到"。
fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 运行一个命令并取其标准输出。命令不存在或失败返回 `None`。
///
/// 只在启动时调用一次，进程开销可忽略；换来的是不必为取一个主机名引入
/// `libc`/`hostname` 依赖。
#[cfg(unix)]
fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 重复发起主持配对时，不得去 bind 一个已被占用的端口。
    ///
    /// 用户第二次点「显示配对码」通常只是想再看一眼码。原先每次点击都新起
    /// 线程去 bind 47685，第二次必然失败并把 `os error 10048` 抛给用户看，
    /// 且只能重启程序恢复。槽位被占用时应返回**当前会话的码**供重新显示。
    #[test]
    fn host_slot_reports_existing_code_instead_of_starting_over() {
        let slot = PairingHostSlot::default();

        let guard = slot.try_acquire().expect("首次应能占用");
        slot.set_code("ABC123");

        match slot.try_acquire() {
            Err(code) => assert_eq!(code, "ABC123", "应返回当前会话的码以便重新显示"),
            Ok(_) => panic!("已有会话时不应再次占用槽位——那会撞上端口占用"),
        }

        // 会话结束后必须能重新发起，否则功能就永久坏掉了。
        drop(guard);
        assert!(
            slot.try_acquire().is_ok(),
            "会话结束后槽位应释放，允许发起新一轮配对"
        );
    }

    /// 槽位在 `host()` 报错返回时也要释放（靠 guard 的 Drop，不能靠成功路径）。
    #[test]
    fn host_slot_releases_on_error_path() {
        let slot = PairingHostSlot::default();
        {
            let _guard = slot.try_acquire().expect("应能占用");
            slot.set_code("XYZ789");
            // 模拟 host() 中途 bail!——guard 在作用域结束时析构。
        }
        assert!(
            slot.try_acquire().is_ok(),
            "出错路径也必须释放槽位，否则一次配对失败就再也发起不了"
        );
    }

    #[test]
    fn non_empty_trims_and_rejects_blank() {
        assert_eq!(non_empty(Some("  mac  ".into())), Some("mac".to_string()));
        assert_eq!(non_empty(Some("   ".into())), None);
        assert_eq!(non_empty(Some(String::new())), None);
        assert_eq!(non_empty(None), None);
    }

    /// 回归：本机必须能取到一个真实设备名。
    ///
    /// 原实现只查 `COMPUTERNAME`/`HOSTNAME`，两者在 macOS 上都不存在
    /// （`HOSTNAME` 是 bash 的 shell 变量，不导出；zsh 不设），于是设备名
    /// 恒为 `unknown-host`——多台 Mac 配对后彼此重名，无法分辨。
    ///
    /// 这条断言在 Windows（`COMPUTERNAME`）与 Unix（`scutil`/`hostname`）
    /// 上都应成立。
    #[test]
    fn device_name_is_not_placeholder_on_this_machine() {
        let name = device_name_best_effort();
        assert_ne!(
            name, "unknown-host",
            "未能取到本机设备名，配对后对端将无法分辨这台机器"
        );
        assert!(!name.trim().is_empty(), "设备名不应为空白");
    }

    /// 设备名不应带 `.local` 之类的域名后缀——展示用，越短越清楚。
    #[cfg(unix)]
    #[test]
    fn unix_device_name_has_no_domain_suffix() {
        // 仅在回退到 `hostname` 这条路径时才需要截断；显式指定或 scutil
        // 的结果本就不带后缀，故这里只断言最终结果不含点分域名形态。
        if std::env::var_os("CLIPSYNC_DEVICE_NAME").is_some() {
            return; // 用户显式指定的名字原样保留，不做断言
        }
        let name = device_name_best_effort();
        assert!(
            !name.ends_with(".local"),
            "设备名残留了 .local 后缀: {name}"
        );
    }

    /// 只有**全部**路径都在落地目录下才算"我们自己刚写入的接收文件"。
    ///
    /// 若只要有一个命中就跳过，用户把收到的文件和自己的文件一起复制时，
    /// 这次真实的复制会被误当作回声而丢失。
    #[test]
    fn received_files_detection_requires_all_paths_inside() {
        use clipsync_clip::ClipRead;
        use clipsync_core::{ClipContent, FileMeta};

        let recv = std::path::PathBuf::from("/tmp/ClipSync/recv");
        let mk = |paths: Vec<&str>| ClipRead {
            content: ClipContent::Files(vec![FileMeta::new("a", 1, 1)]),
            sensitive: false,
            file_paths: paths.into_iter().map(std::path::PathBuf::from).collect(),
        };

        assert!(is_our_received_files(
            &mk(vec!["/tmp/ClipSync/recv/0001/a.txt"]),
            &recv
        ));
        assert!(!is_our_received_files(
            &mk(vec!["/tmp/ClipSync/recv/0001/a.txt", "/Users/me/b.txt"]),
            &recv
        ));
        assert!(!is_our_received_files(&mk(vec![]), &recv));
    }
}
