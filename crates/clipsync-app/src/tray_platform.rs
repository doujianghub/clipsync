//! 托盘所需的平台原生消息处理。
//!
//! 两个平台都要求托盘所属线程持有消息循环，否则图标出得来、菜单点不动。

/// 处理平台原生消息，使托盘图标与菜单能够响应。
#[cfg(windows)]
pub(super) fn pump_platform_events() {
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
pub(super) fn pump_platform_events() {
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
pub(super) fn init_platform_app() {
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
pub(super) fn init_platform_app() {}

#[cfg(not(any(windows, target_os = "macos")))]
pub(super) fn pump_platform_events() {
    // 其它平台无需额外的消息泵。
}
