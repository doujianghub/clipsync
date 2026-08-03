//! 廉价的剪贴板格式查询。
//!
//! 读取剪贴板内容需要"打开"剪贴板——这是独占操作，会与其它程序竞争锁。为了
//! 尽量不干扰系统自带的复制粘贴，先用**不需要打开剪贴板**的系统调用查明当前
//! 有哪些格式，再决定是否真的去读，从而把打开次数降到最低。
//!
//! 例如：剪贴板里只有文本时，直接跳过"尝试读图片"这一步，少一次打开。

/// 当前剪贴板是否含有位图。
///
/// `Some(true/false)` 为确切结论；`None` 表示本平台无廉价查询手段，调用方
/// 应按原有逻辑尝试读取。
pub fn clipboard_has_image() -> Option<bool> {
    platform::has_image()
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::System::DataExchange::IsClipboardFormatAvailable;

    /// 设备无关位图。绝大多数程序复制图片时都会提供此格式。
    const CF_DIB: u32 = 8;
    /// BITMAPV5 格式（含 alpha 通道），较新程序使用。
    const CF_DIBV5: u32 = 17;
    /// 位图句柄格式。
    const CF_BITMAP: u32 = 2;

    pub fn has_image() -> Option<bool> {
        // SAFETY: 纯查询调用，不打开剪贴板、不获取所有权、不写入内存。
        let present = unsafe {
            IsClipboardFormatAvailable(CF_DIB) != 0
                || IsClipboardFormatAvailable(CF_DIBV5) != 0
                || IsClipboardFormatAvailable(CF_BITMAP) != 0
        };
        Some(present)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2_app_kit::NSPasteboard;

    /// 截图、预览等写入的标准图片类型（UTI）。
    /// TIFF 是 macOS 的传统位图类型，多数程序都会提供；PNG 次之。
    const PNG: &str = "public.png";
    const TIFF: &str = "public.tiff";

    pub fn has_image() -> Option<bool> {
        // 只查类型列表，不取内容——不打开剪贴板，不与其它程序抢锁。
        let pb = NSPasteboard::generalPasteboard();
        let types = pb.types()?; // None：剪贴板为空，无从判断，交回调用方按原逻辑处理
        Some(
            types
                .iter()
                .any(|t| matches!(t.to_string().as_str(), PNG | TIFF)),
        )
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    // 其它平台暂无廉价查询手段。返回 None 表示按原逻辑尝试读取，功能不受影响。
    pub fn has_image() -> Option<bool> {
        None
    }
}
