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
pub use tray_status::TrayStatus;

use tray_platform::{init_platform_app, pump_platform_events};

#[path = "tray_menu.rs"]
mod tray_menu;

pub(crate) use tray_menu::human_bytes;
use tray_menu::{max_bytes_label, port_label, rate_label, rebuild_peer_menu};

// 不再是 Copy：`Unpair` 携带 device id。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    TogglePause,
    ToggleAutostart,
    /// 主持配对：生成并显示配对码，等对方连入。
    ShowPairingCode,
    /// 加入配对：输入对方给的配对码，主动连过去。
    EnterPairingCode,
    Quit,
    /// 切换是否同步图片（收发都受此开关约束）。
    ToggleImages,
    /// 切换是否同步文件。
    ToggleFiles,
    /// 切换传输前自动压缩。
    ToggleCompress,
    /// 弹输入框改单次大小上限。
    PromptMaxBytes,
    /// 弹输入框改发送限速。
    PromptUploadLimit,
    /// 弹输入框改同步监听端口。
    PromptListenPort,
    /// 解除与某台设备的配对（携带其 device id）。
    Unpair(String),
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
}

/// 托盘需要展示的当前设置值（用于菜单初始勾选状态）。
#[derive(Debug, Clone, Copy)]
pub struct TraySettings {
    pub sync_images: bool,
    pub sync_files: bool,
    pub compress: bool,
    pub max_bytes: usize,
    pub upload_limit: u64,
    pub listen_port: u16,
    pub verbose_log: bool,
}

/// 托盘运行所需的回调。
pub struct TrayCallbacks {
    /// 处理一次菜单动作；返回 false 表示应退出程序。
    pub on_action: Box<dyn FnMut(TrayAction) -> bool>,
    /// 读取当前设置，用于点击后刷新勾选状态（以实际结果为准，避免脱节）。
    pub current_settings: Box<dyn Fn() -> TraySettings>,
    /// 读取当前已配对设备及其在线状态，用于渲染设备子菜单。
    pub current_peers: Box<dyn Fn() -> Vec<TrayPeer>>,
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
    let pair_item = MenuItem::new("显示配对码…", true, None);
    let join_item = MenuItem::new("输入配对码…", true, None);
    let peers_menu = Submenu::new("已配对设备", true);
    let mut peer_items = rebuild_peer_menu(&peers_menu, &[], &(callbacks.current_peers)())?;

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
    let max_item = MenuItem::new(max_bytes_label(s0.max_bytes), true, None);
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

    menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &pair_item,
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

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(status.summary())
        .with_icon(make_icon(IconState::of(&status))?)
        .build()
        .map_err(|e| anyhow::anyhow!("创建托盘图标失败: {e}"))?;

    let menu_rx = MenuEvent::receiver();
    let mut last_summary = status.summary();

    let mut current_icon = IconState::of(&status);

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
                Some(TrayAction::PromptMaxBytes)
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
                    .map(|(_, device)| TrayAction::Unpair(device.clone()))
            };

            if let Some(a) = action {
                let keep_running = (callbacks.on_action)(a);
                if !keep_running {
                    return Ok(());
                }
                // 勾选状态一律以**实际生效值**为准，而不是按点击取反：
                // 保存失败或值被夹取时，界面不会与真实状态脱节。
                pause_item.set_checked(status.is_paused());
                autostart_item.set_checked(crate::autostart::is_enabled());

                let s = (callbacks.current_settings)();
                images_item.set_checked(s.sync_images);
                files_item.set_checked(s.sync_files);
                compress_item.set_checked(s.compress);
                verbose_item.set_checked(s.verbose_log);
                // 标签里带着当前值，改完要跟着变。
                max_item.set_text(max_bytes_label(s.max_bytes));
                rate_item.set_text(rate_label(s.upload_limit));
                port_item.set_text(port_label(s.listen_port));
                // 解除配对会改变设备列表，重建一次。
                peer_items =
                    rebuild_peer_menu(&peers_menu, &peer_items, &(callbacks.current_peers)())?;
            }
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
            let _ = tray.set_tooltip(Some(&summary));
            last_summary = summary;
            // 汇总变了意味着连接数变了，设备子菜单里的 ●/○ 也该跟着变。
            peer_items = rebuild_peer_menu(&peers_menu, &peer_items, &(callbacks.current_peers)())?;
        }
        let icon_state = IconState::of(&status);
        if icon_state != current_icon {
            if let Ok(icon) = make_icon(icon_state) {
                let _ = tray.set_icon(Some(icon));
            }
            current_icon = icon_state;
        }

        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_reflects_state() {
        let s = TrayStatus::new(0);
        assert!(s.summary().contains("尚未配对"));

        let s = TrayStatus::new(2);
        assert!(s.summary().contains("未连接"));

        s.set_connected_ids(["dev-a".to_string()].into_iter().collect());
        assert!(s.summary().contains("已连接 1 / 2"));

        s.set_paused(true);
        assert!(s.summary().contains("已暂停"), "暂停状态应优先展示");
    }

    /// 中枢停止后，界面不得再显示"一切正常"。
    ///
    /// 同步已经彻底不工作了，而托盘图标还是绿的、菜单还写着"已连接 N 台"
    /// ——用户对着这个界面永远想不通"为什么复制过不去"。
    #[test]
    fn dead_hub_overrides_healthy_looking_state() {
        let s = TrayStatus::new(2);
        s.set_connected_ids(["dev-a".to_string()].into_iter().collect());
        assert_eq!(IconState::of(&s), IconState::Connected);

        s.set_hub_dead();
        assert_eq!(
            IconState::of(&s),
            IconState::Broken,
            "中枢已死时不能还显示已连接"
        );
        assert!(
            s.summary().contains("同步已停止"),
            "文字也要如实说明，实际: {}",
            s.summary()
        );
    }

    /// 故障优先于暂停：两者都成立时该显示故障，暂停是用户自己知道的事。
    #[test]
    fn broken_takes_precedence_over_paused() {
        let s = TrayStatus::new(1);
        s.set_paused(true);
        s.set_hub_dead();
        assert_eq!(IconState::of(&s), IconState::Broken);
    }

    #[test]
    fn pause_state_is_shared_across_clones() {
        let s = TrayStatus::new(1);
        let clone = s.clone();
        s.set_paused(true);
        assert!(
            clone.is_paused(),
            "克隆出的句柄应看到同一份暂停状态（中枢与托盘共享）"
        );
    }
}
