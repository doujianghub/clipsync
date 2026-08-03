//! 系统托盘：状态显示与常用操作入口。
//!
//! 托盘是本程序唯一的界面。设计原则是"平时不打扰、需要时找得到"：
//!   - 图标颜色即状态（已连接/未连接/已暂停），一眼可知同步是否正常。
//!   - 菜单只放真正需要的操作：配对、暂停、开机自启、退出。
//!
//! **图标由代码生成**而非打包图片文件，这样发布物始终是单个可执行文件，
//! 也免去了不同平台的资源打包差异。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tray_icon::Icon;

/// 同步状态，供托盘展示。
#[derive(Debug, Clone, Default)]
pub struct StatusData {
    /// 当前已连接的对端数。
    pub connected: usize,
    /// 已配对设备总数。
    pub paired: usize,
}

/// 线程安全的状态句柄：中枢更新，托盘读取。
#[derive(Clone, Default)]
pub struct TrayStatus {
    data: Arc<Mutex<StatusData>>,
    paused: Arc<AtomicBool>,
}

impl TrayStatus {
    pub fn new(paired: usize) -> Self {
        Self {
            data: Arc::new(Mutex::new(StatusData {
                connected: 0,
                paired,
            })),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_connected(&self, n: usize) {
        self.data.lock().unwrap().connected = n;
    }

    pub fn snapshot(&self) -> StatusData {
        self.data.lock().unwrap().clone()
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    /// 一句话状态描述，用于托盘提示文本。
    pub fn summary(&self) -> String {
        if self.is_paused() {
            return "ClipSync — 已暂停".to_string();
        }
        let s = self.snapshot();
        if s.paired == 0 {
            "ClipSync — 尚未配对设备".to_string()
        } else if s.connected == 0 {
            format!("ClipSync — 未连接（已配对 {} 台）", s.paired)
        } else {
            format!("ClipSync — 已连接 {} / {} 台", s.connected, s.paired)
        }
    }
}

/// 托盘图标的三种视觉状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    /// 至少一台设备已连接——同步正常。
    Connected,
    /// 无设备连接。
    Disconnected,
    /// 用户主动暂停。
    Paused,
}

impl IconState {
    pub fn of(status: &TrayStatus) -> Self {
        if status.is_paused() {
            IconState::Paused
        } else if status.snapshot().connected > 0 {
            IconState::Connected
        } else {
            IconState::Disconnected
        }
    }

    /// 该状态对应的主色（RGB）。
    fn color(self) -> (u8, u8, u8) {
        match self {
            // 绿：一切正常
            IconState::Connected => (0x35, 0xB5, 0x6A),
            // 灰：未连接
            IconState::Disconnected => (0x8A, 0x8A, 0x8A),
            // 琥珀：已暂停
            IconState::Paused => (0xE0, 0xA0, 0x30),
        }
    }
}

/// 图标边长（像素）。
const ICON_SIZE: u32 = 32;

/// 按状态生成托盘图标：一个简化的剪贴板轮廓。
pub fn make_icon(state: IconState) -> anyhow::Result<Icon> {
    let rgba = draw_clipboard(state);
    Icon::from_rgba(rgba, ICON_SIZE, ICON_SIZE)
        .map_err(|e| anyhow::anyhow!("生成托盘图标失败: {e}"))
}

/// 绘制剪贴板形状的 RGBA 像素。
///
/// 形状：一个圆角板身，顶部一个夹子。用纯计算绘制，无需图片资源。
fn draw_clipboard(state: IconState) -> Vec<u8> {
    let (r, g, b) = state.color();
    let n = ICON_SIZE as i32;
    let mut px = vec![0u8; (ICON_SIZE * ICON_SIZE * 4) as usize];

    // 板身范围（留出边距）与夹子范围。
    let body = Rect {
        x0: 6,
        y0: 7,
        x1: n - 6,
        y1: n - 4,
    };
    let clip = Rect {
        x0: n / 2 - 5,
        y0: 3,
        x1: n / 2 + 5,
        y1: 9,
    };

    for y in 0..n {
        for x in 0..n {
            let idx = ((y * n + x) * 4) as usize;
            let in_body = body.contains_rounded(x, y, 3);
            let in_clip = clip.contains_rounded(x, y, 2);

            if in_clip {
                // 夹子用更深的同色，形成层次。
                px[idx] = r.saturating_sub(40);
                px[idx + 1] = g.saturating_sub(40);
                px[idx + 2] = b.saturating_sub(40);
                px[idx + 3] = 255;
            } else if in_body {
                px[idx] = r;
                px[idx + 1] = g;
                px[idx + 2] = b;
                px[idx + 3] = 255;
            }
            // 其余保持全透明。
        }
    }
    px
}

struct Rect {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl Rect {
    /// 是否落在带圆角的矩形内。
    fn contains_rounded(&self, x: i32, y: i32, radius: i32) -> bool {
        if x < self.x0 || x >= self.x1 || y < self.y0 || y >= self.y1 {
            return false;
        }
        // 四角做圆角裁切。
        let corners = [
            (self.x0 + radius, self.y0 + radius),
            (self.x1 - 1 - radius, self.y0 + radius),
            (self.x0 + radius, self.y1 - 1 - radius),
            (self.x1 - 1 - radius, self.y1 - 1 - radius),
        ];
        for (cx, cy) in corners {
            let outside_x = (x < cx && cx == self.x0 + radius) || (x > cx && cx != self.x0 + radius);
            let outside_y = (y < cy && cy == self.y0 + radius) || (y > cy && cy != self.y0 + radius);
            if outside_x && outside_y {
                let dx = x - cx;
                let dy = y - cy;
                if dx * dx + dy * dy > radius * radius {
                    return false;
                }
            }
        }
        true
    }
}

/// 托盘菜单被点击后需要主程序执行的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    TogglePause,
    ToggleAutostart,
    ShowPairingCode,
    Quit,
}

/// 托盘运行所需的回调。
pub struct TrayCallbacks {
    /// 处理一次菜单动作；返回 false 表示应退出程序。
    pub on_action: Box<dyn FnMut(TrayAction) -> bool>,
}

/// 在**主线程**上创建托盘并运行事件循环，直到用户选择退出。
///
/// 必须在主线程运行：macOS 要求 UI 操作在主线程，Windows 也要求托盘图标所属
/// 线程持有消息循环。因此同步逻辑全部放在后台线程，主线程专职跑这个循环。
pub fn run(status: TrayStatus, mut callbacks: TrayCallbacks) -> anyhow::Result<()> {
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::TrayIconBuilder;

    // 平台初始化需先于托盘构建：macOS 上 NSApp 未完成启动时创建的状态项
    // 不会响应点击。
    init_platform_app();

    let menu = Menu::new();
    // 首项显示状态，不可点击，仅作信息展示。
    let status_item = MenuItem::new(status.summary(), false, None);
    let pair_item = MenuItem::new("显示配对码…", true, None);
    let pause_item = CheckMenuItem::new("暂停同步", true, status.is_paused(), None);
    let autostart_item =
        CheckMenuItem::new("开机自启", true, crate::autostart::is_enabled(), None);
    let quit_item = MenuItem::new("退出 ClipSync", true, None);

    menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &pair_item,
        &PredefinedMenuItem::separator(),
        &pause_item,
        &autostart_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])
    .map_err(|e| anyhow::anyhow!("构建托盘菜单失败: {e}"))?;

    let mut current_icon = IconState::of(&status);
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(status.summary())
        .with_icon(make_icon(current_icon)?)
        .build()
        .map_err(|e| anyhow::anyhow!("创建托盘图标失败: {e}"))?;

    let menu_rx = MenuEvent::receiver();
    let mut last_summary = status.summary();

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
            } else if event.id == quit_item.id() {
                Some(TrayAction::Quit)
            } else {
                None
            };

            if let Some(a) = action {
                let keep_running = (callbacks.on_action)(a);
                // 勾选状态以实际结果为准，避免与真实状态脱节。
                pause_item.set_checked(status.is_paused());
                autostart_item.set_checked(crate::autostart::is_enabled());
                if !keep_running {
                    return Ok(());
                }
            }
        }

        // 状态变化时刷新图标与文字。
        let summary = status.summary();
        if summary != last_summary {
            status_item.set_text(&summary);
            let _ = tray.set_tooltip(Some(&summary));
            last_summary = summary;
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

/// 处理平台原生消息，使托盘图标与菜单能够响应。
#[cfg(windows)]
fn pump_platform_events() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    // SAFETY: 标准的非阻塞消息泵。PeekMessage 取不到消息时立即返回 0。
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(target_os = "macos")]
fn pump_platform_events() {
    use objc2_app_kit::{NSApplication, NSEventMask};
    use objc2_foundation::{MainThreadMarker, NSDate, NSDefaultRunLoopMode};

    // 托盘必须在主线程运行（见 `run` 的文档）。非主线程时静默返回而不 panic：
    // 事件泵取不到事件只会让菜单无响应，不该让整个程序崩溃。
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);

    // distantPast 作为超时点表示"绝不等待"：有事件就取走，没有立即返回 None。
    // 这样循环不会阻塞，主线程仍能按 200ms 节奏刷新图标与处理菜单事件。
    // SAFETY: NSDefaultRunLoopMode 是 AppKit 导出的常量字符串，读取始终有效。
    let mode = unsafe { NSDefaultRunLoopMode };
    while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
        NSEventMask::Any,
        Some(&NSDate::distantPast()),
        mode,
        true,
    ) {
        app.sendEvent(&event);
    }
}

/// macOS 专用：进入事件循环前初始化 NSApp。
///
/// 两件事缺一不可：
///   - `Accessory` 激活策略——托盘程序不应在 Dock 里占一个图标。
///   - `finishLaunching`——不调用则 AppKit 未完成启动流程，菜单点击无响应。
#[cfg(target_os = "macos")]
fn init_platform_app() {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use objc2_foundation::MainThreadMarker;

    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("托盘未在主线程启动，菜单可能无响应");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
}

#[cfg(not(target_os = "macos"))]
fn init_platform_app() {}

#[cfg(not(any(windows, target_os = "macos")))]
fn pump_platform_events() {
    // 其它平台无需额外的消息泵。
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

        s.set_connected(1);
        assert!(s.summary().contains("已连接 1 / 2"));

        s.set_paused(true);
        assert!(s.summary().contains("已暂停"), "暂停状态应优先展示");
    }

    #[test]
    fn icon_state_priority() {
        let s = TrayStatus::new(1);
        assert_eq!(IconState::of(&s), IconState::Disconnected);

        s.set_connected(1);
        assert_eq!(IconState::of(&s), IconState::Connected);

        // 暂停优先于连接状态——用户主动暂停时应明确显示。
        s.set_paused(true);
        assert_eq!(IconState::of(&s), IconState::Paused);
    }

    #[test]
    fn icon_pixels_have_expected_size_and_content() {
        let px = draw_clipboard(IconState::Connected);
        assert_eq!(px.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        // 应有不透明像素（画出了图形），也应有透明像素（四周留白）。
        assert!(px.chunks(4).any(|p| p[3] == 255), "应绘制出可见图形");
        assert!(px.chunks(4).any(|p| p[3] == 0), "四周应为透明");
    }

    #[test]
    fn different_states_produce_different_icons() {
        let a = draw_clipboard(IconState::Connected);
        let b = draw_clipboard(IconState::Disconnected);
        let c = draw_clipboard(IconState::Paused);
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
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
