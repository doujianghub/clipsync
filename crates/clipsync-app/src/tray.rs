//! 系统托盘：状态显示与常用操作入口。
//!
//! 托盘是本程序唯一的界面。设计原则是"平时不打扰、需要时找得到"：
//!   - 图标颜色即状态（已连接/未连接/已暂停），一眼可知同步是否正常。
//!   - 菜单只放真正需要的操作：配对、暂停、开机自启、退出。
//!
//! **图标由代码生成**而非打包图片文件，这样发布物始终是单个可执行文件，
//! 也免去了不同平台的资源打包差异。

// 子模块文件与本文件平级，故显式指路（否则 Rust 会去找 src/tray/ 目录）。
#[path = "tray_icon_draw.rs"]
mod tray_icon_draw;
#[path = "tray_platform.rs"]
mod tray_platform;
#[path = "tray_status.rs"]
mod tray_status;

pub use tray_icon_draw::{make_icon, IconState};
pub use tray_status::{PendingFetchInfo, TrayStatus, TransferProgress};
pub(crate) use tray_status::ellipsize_middle;

use tray_platform::{init_platform_app, pump_platform_events};

#[path = "tray_menu.rs"]
mod tray_menu;

pub(crate) use tray_menu::human_bytes;
use tray_menu::{
    auto_fetch_label, fetch_label, pairing_label, port_label, rate_label, rebuild_peer_menu,
    LEAVE_GROUP_ID,
};

// 不再是 Copy：`RemovePeer` 携带 device id。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    TogglePause,
    ToggleAutostart,
    /// 主持配对：生成并显示配对码，等对方连入。
    ShowPairingCode,
    /// 加入配对：输入对方给的配对码，主动连过去。
    EnterPairingCode,
    /// 作废当前配对码，换一个新的。
    NewPairingCode,
    /// 取回挂起的大文件（超过自动取回上限、没有自动拉的那批）。
    FetchPending,
    Quit,
    /// 切换是否同步图片（收发都受此开关约束）。
    ToggleImages,
    /// 切换是否同步文件。
    ToggleFiles,
    /// 切换传输前自动压缩。
    ToggleCompress,
    /// 弹输入框改自动取回上限。
    PromptAutoFetch,
    /// 弹输入框改发送限速。
    PromptUploadLimit,
    /// 弹输入框改同步监听端口。
    PromptListenPort,
    /// 把某台设备移出设备组（携带其 device id），全组生效。
    RemovePeer(String),
    /// 本机退出设备组：清空全部配对，并告知其它成员。
    LeaveGroup,
    /// 打开日志所在文件夹。
    OpenLogDir,
    /// 切换详细日志（info ↔ debug）。
    ToggleVerboseLog,
}

/// 托盘要展示的一台已配对设备。
#[derive(Debug, Clone)]
pub struct TrayPeer {
    pub device: String,
    pub name: String,
    pub online: bool,
    /// 由哪台设备引荐而来；`None` 表示用户亲手配对。
    pub introduced_by: Option<String>,
}

/// 托盘需要展示的当前设置值（用于菜单初始勾选状态）。
#[derive(Debug, Clone, Copy)]
pub struct TraySettings {
    pub sync_images: bool,
    pub sync_files: bool,
    pub compress: bool,
    pub auto_fetch_bytes: usize,
    pub upload_limit: u64,
    pub listen_port: u16,
    pub verbose_log: bool,
}

/// 托盘运行所需的回调。
pub struct TrayCallbacks {
    /// 处理一次菜单动作；返回 false 表示应退出程序。
    pub on_action: Box<dyn FnMut(TrayAction) -> bool>,
    /// 读取当前设置，用于刷新勾选与带值的标签（以实际结果为准，避免脱节）。
    pub current_settings: Box<dyn Fn() -> TraySettings>,
    /// 设置的变更计数。变了才重渲染，省掉每轮一次的快照克隆。
    ///
    /// **为什么要轮询而不是点完就刷**：改设置的弹窗一律跑在后台线程（不然
    /// 整个菜单会卡住），`on_action` 在用户还没看见窗口时就返回了。点完立刻
    /// 刷等于把**改之前**的值又渲染一遍——实机上的表现是"选了 100 MiB，
    /// 菜单还写着不限；下次再选，才显示上一次选的值"，晚一拍。
    pub settings_version: Box<dyn Fn() -> u64>,
    /// 读取当前已配对设备及其在线状态，用于渲染设备子菜单。
    pub current_peers: Box<dyn Fn() -> Vec<TrayPeer>>,
    /// 设备表的变更计数。配对、引荐、移出都会让它变。
    pub peers_version: Box<dyn Fn() -> u64>,
    /// 当前有效的配对码与剩余秒数；没有会话时为 `None`。
    ///
    /// 菜单是唯一能实时更新的地方——弹窗一显示文字就定死了。
    pub live_pairing_code: Box<dyn Fn() -> Option<(String, u64)>>,
    /// 同步中枢是否仍在运行。托盘每轮询问一次；一旦为 false 就切到故障状态。
    pub hub_alive: Box<dyn Fn() -> bool>,
}

/// 在**主线程**上创建托盘并运行事件循环，直到用户选择退出。
///
/// 必须在主线程运行：macOS 要求 UI 操作在主线程，Windows 也要求托盘图标所属
/// 线程持有消息循环。因此同步逻辑全部放在后台线程，主线程专职跑这个循环。
pub fn run(status: TrayStatus, mut callbacks: TrayCallbacks) -> anyhow::Result<()> {
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
    use tray_icon::TrayIconBuilder;

    // 平台初始化需先于托盘构建：macOS 上 NSApp 未完成启动时创建的状态项
    // 不会响应点击。
    init_platform_app();

    let s0 = (callbacks.current_settings)();

    let menu = Menu::new();
    // 首项显示状态，不可点击，仅作信息展示。
    let status_item = MenuItem::new(status.summary(), false, None);

    // —— 一级：日常会碰的 ——
    //
    // 配对那一项在会话进行中会变成实时倒计时（`配对码 1234 · 剩 2:47`），
    // 点它仍是"把窗口再调出来"。
    let mut live_code = (callbacks.live_pairing_code)();
    let pair_item = MenuItem::new(
        pairing_label(live_code.as_ref().map(|(c, s)| (c.as_str(), *s))),
        true,
        None,
    );
    // 没有会话时**保留但禁用**，不隐藏：菜单少一行会让下面的项跟着上移，
    // 点惯了位置的人会点错；灰着也一眼能看出"现在没码可换"。
    let new_code_item = MenuItem::new(
        crate::pairing_ui::NEW_CODE_LABEL,
        live_code.is_some(),
        None,
    );
    let join_item = MenuItem::new("输入配对码…", true, None);
    let peers_menu = Submenu::new("已配对设备", true);
    let mut peer_items = rebuild_peer_menu(&peers_menu, &(callbacks.current_peers)())?;

    // 开关收发两侧都生效，所以可以就叫"同步图片"（见 engine 的 kind_disabled）。
    let images_item = CheckMenuItem::new("同步图片", true, s0.sync_images, None);
    let files_item = CheckMenuItem::new("同步文件", true, s0.sync_files, None);
    let pause_item = CheckMenuItem::new("暂停同步", true, status.is_paused(), None);
    let autostart_item = CheckMenuItem::new("开机自启", true, crate::autostart::is_enabled(), None);

    // —— 二级：少数人偶尔需要 ——
    //
    // 分层而非直接砍掉：这些确实有人要用，但摆在一级会让每天都看见它的
    // 多数人多扫四行。收进来后一级只剩日常项，需要时也找得到——比让人去
    // 手工编辑 settings.json 强得多。
    //
    // 可调项一律把当前值写进标签，点击即弹输入框；不再给预设档——有了
    // 输入框，五个档位既占地方又永远不够用。
    let max_item = MenuItem::new(auto_fetch_label(s0.auto_fetch_bytes), true, None);
    let rate_item = MenuItem::new(rate_label(s0.upload_limit), true, None);
    let port_item = MenuItem::new(port_label(s0.listen_port), true, None);
    let compress_item = CheckMenuItem::new("传输前压缩", true, s0.compress, None);
    let verbose_item = CheckMenuItem::new("详细日志", true, s0.verbose_log, None);
    let log_dir_item = MenuItem::new("打开日志文件夹…", true, None);

    let advanced_menu = Submenu::new("高级设置", true);
    advanced_menu
        .append_items(&[
            &max_item,
            &rate_item,
            &port_item,
            &compress_item,
            &PredefinedMenuItem::separator(),
            &verbose_item,
            &log_dir_item,
        ])
        .map_err(|e| anyhow::anyhow!("构建高级设置子菜单失败: {e}"))?;

    let quit_item = MenuItem::new("退出", true, None);

    // 「取回」是**待办通知**，不是常驻入口：没有待取项时整行不存在。
    //
    // 与「换个配对码」不同——那一项是配对入口的近邻，藏起来会让人找不着，
    // 所以留着灰掉。这一项则相反：它出现本身就是信息，常驻一行灰字既没内容
    // 又占地方。位置固定在首行下方，一有东西就在最显眼的地方。
    let fetch_item = MenuItem::new("取回", true, None);
    let mut fetch_shown = false;
    /// `fetch_item` 插入的位置：状态行 + 分隔符之后。
    const FETCH_SLOT: usize = 2;

    menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &pair_item,
        &new_code_item,
        &join_item,
        &peers_menu,
        &PredefinedMenuItem::separator(),
        &images_item,
        &files_item,
        &pause_item,
        &PredefinedMenuItem::separator(),
        &autostart_item,
        &advanced_menu,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])
    .map_err(|e| anyhow::anyhow!("构建托盘菜单失败: {e}"))?;

    // 菜单要在运行期增删「取回」那一行，而 builder 会把它拿走——留一份句柄。
    // `Menu` 内部是共享引用，克隆出来指的是同一个菜单。
    let menu_handle = menu.clone();
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(status.tooltip())
        .with_icon(make_icon(IconState::of(&status), false, status.pending().is_some())?)
        .build()
        .map_err(|e| anyhow::anyhow!("创建托盘图标失败: {e}"))?;

    let menu_rx = MenuEvent::receiver();
    let mut last_summary = status.summary();

    // 当前画着的图标：状态 + 脉冲相位。两者任一变化才重画——托盘循环每
    // 200ms 转一圈，无脑重画等于一秒生成五张图标，白费 CPU。
    let mut current_icon = (IconState::of(&status), false, status.pending().is_some());
    let mut last_pending = None;
    let loop_started = std::time::Instant::now();
    let mut last_connected = status.connected_devices();
    let mut last_settings_version = (callbacks.settings_version)();
    let mut last_peers_version = (callbacks.peers_version)();

    loop {
        // 让平台处理其自身的窗口/菜单消息。
        pump_platform_events();

        // 处理菜单点击。
        while let Ok(event) = menu_rx.try_recv() {
            let action = if event.id == pause_item.id() {
                Some(TrayAction::TogglePause)
            } else if event.id == autostart_item.id() {
                Some(TrayAction::ToggleAutostart)
            } else if event.id == pair_item.id() {
                Some(TrayAction::ShowPairingCode)
            } else if event.id == fetch_item.id() {
                Some(TrayAction::FetchPending)
            } else if event.id == new_code_item.id() {
                Some(TrayAction::NewPairingCode)
            } else if event.id == join_item.id() {
                Some(TrayAction::EnterPairingCode)
            } else if event.id == quit_item.id() {
                Some(TrayAction::Quit)
            } else if event.id == images_item.id() {
                Some(TrayAction::ToggleImages)
            } else if event.id == files_item.id() {
                Some(TrayAction::ToggleFiles)
            } else if event.id == compress_item.id() {
                Some(TrayAction::ToggleCompress)
            } else if event.id == max_item.id() {
                Some(TrayAction::PromptAutoFetch)
            } else if event.id == rate_item.id() {
                Some(TrayAction::PromptUploadLimit)
            } else if event.id == port_item.id() {
                Some(TrayAction::PromptListenPort)
            } else if event.id == verbose_item.id() {
                Some(TrayAction::ToggleVerboseLog)
            } else if event.id == log_dir_item.id() {
                Some(TrayAction::OpenLogDir)
            } else {
                peer_items
                    .iter()
                    .find(|(i, _)| i.id() == &event.id)
                    .map(|(_, device)| {
                        if device == LEAVE_GROUP_ID {
                            TrayAction::LeaveGroup
                        } else {
                            TrayAction::RemovePeer(device.clone())
                        }
                    })
            };

            if let Some(a) = action {
                let keep_running = (callbacks.on_action)(a);
                if !keep_running {
                    return Ok(());
                }
                // 这两项就地生效（不弹窗、不起线程），点完即可刷新；且一律以
                // **实际生效值**为准而不是按点击取反——设置失败时界面不会撒谎。
                // 其余项改在后台线程里，交给下面的版本号轮询。
                pause_item.set_checked(status.is_paused());
                autostart_item.set_checked(crate::autostart::is_enabled());
            }
        }

        // 待取项变了：换标签，并按需把整行插进来或摘掉。
        let pending_now = status.pending();
        if pending_now != last_pending {
            match &pending_now {
                Some(p) => {
                    fetch_item.set_text(fetch_label(&p.first_name, p.count, p.total));
                    if !fetch_shown {
                        menu_handle.insert(&fetch_item, FETCH_SLOT)
                            .map_err(|e| anyhow::anyhow!("插入取回菜单项失败: {e}"))?;
                        fetch_shown = true;
                    }
                }
                None => {
                    if fetch_shown {
                        menu_handle.remove(&fetch_item)
                            .map_err(|e| anyhow::anyhow!("移除取回菜单项失败: {e}"))?;
                        fetch_shown = false;
                    }
                }
            }
            last_pending = pending_now;
        }

        // 配对码倒计时：只在**显示出来的那个数**变了才动菜单，也就是每秒
        // 一次而不是每轮五次。
        let live_now = (callbacks.live_pairing_code)();
        if live_now != live_code {
            pair_item.set_text(pairing_label(
                live_now.as_ref().map(|(c, s)| (c.as_str(), *s)),
            ));
            // 会话起止时才需要动 enabled，但一次布尔赋值比判断它变没变还便宜。
            new_code_item.set_enabled(live_now.is_some());
            live_code = live_now;
        }

        // 设置变了就重渲染。改动多半来自后台线程里的弹窗，点击那一刻还没发生。
        let settings_now = (callbacks.settings_version)();
        if settings_now != last_settings_version {
            last_settings_version = settings_now;
            let s = (callbacks.current_settings)();
            images_item.set_checked(s.sync_images);
            files_item.set_checked(s.sync_files);
            compress_item.set_checked(s.compress);
            verbose_item.set_checked(s.verbose_log);
            // 标签里带着当前值，改完要跟着变。
            max_item.set_text(auto_fetch_label(s.auto_fetch_bytes));
            rate_item.set_text(rate_label(s.upload_limit));
            port_item.set_text(port_label(s.listen_port));
        }

        // 中枢若已停止，同步实际已经不工作了——必须让界面如实反映，
        // 否则用户对着一个绿图标怎么也想不通"为什么复制过不去"。
        if !status.is_hub_dead() && !(callbacks.hub_alive)() {
            status.set_hub_dead();
        }

        // 状态变化时刷新图标与文字。
        let summary = status.summary();
        if summary != last_summary {
            status_item.set_text(&summary);
            // 提示与菜单项**不是**同一份文案：Windows 的托盘提示只有 63 个
            // 字符，放不下菜单项那份带绝对字节数的详细版。
            let _ = tray.set_tooltip(Some(&status.tooltip()));
            last_summary = summary;
        }

        // 设备子菜单在**成员**或**在线集合**变化时重建——前者决定列几行，
        // 后者决定每行是 ● 还是 ○。成员变化同样可能来自别处：引荐认识一台新
        // 设备、对端广播把某台移出，都不经过本机的菜单点击。
        //
        // 只认这两件事，不跟着摘要文案走：加了传输进度之后摘要每 200ms 就变
        // 一次（百分比、速度都在动），跟着重建等于一秒五次，白费 CPU，菜单正
        // 开着的话还会闪。
        let connected_now = status.connected_devices();
        let peers_now = (callbacks.peers_version)();
        if connected_now != last_connected || peers_now != last_peers_version {
            last_connected = connected_now;
            last_peers_version = peers_now;
            peer_items = rebuild_peer_menu(&peers_menu, &(callbacks.current_peers)())?;
        }
        // 传输中让图标脉冲：亮 / 淡各 450ms。周期取得比 200ms 的循环间隔长
        // 得多，免得相位被采样节奏切碎而看起来在抖。
        const PULSE_HALF_PERIOD_MS: u128 = 450;
        let dimmed = status.is_transferring()
            && (loop_started.elapsed().as_millis() / PULSE_HALF_PERIOD_MS) % 2 == 1;
        let icon_state = (IconState::of(&status), dimmed, last_pending.is_some());
        if icon_state != current_icon {
            if let Ok(icon) = make_icon(icon_state.0, icon_state.1, icon_state.2) {
                let _ = tray.set_icon(Some(icon));
            }
            current_icon = icon_state;
        }

        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[cfg(test)]
#[path = "tray_tests.rs"]
mod tests;
