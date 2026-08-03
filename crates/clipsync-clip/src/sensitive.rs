//! 敏感/瞬态剪贴板内容探测。
//!
//! 目标：尊重密码管理器等工具的"不要记录此剪贴板内容"标记，自动跳过同步，
//! 避免把密码等敏感数据广播到其他设备。
//!
//! 各平台约定的标记方式不同：
//!   - **Windows**：应用在剪贴板上放置一个空的自定义格式，格式名为
//!     `ExcludeClipboardContentFromMonitorProcessing`（微软官方约定，剪贴板
//!     历史/云剪贴板据此排除）或 `CanIncludeInClipboardHistory`（值为 0 时排除）。
//!     KeePass、1Password 等均遵循。我们只需检测这些格式是否存在。
//!   - **macOS**：应用在 `NSPasteboard` 上写入 `org.nspasteboard.ConcealedType`
//!     或 `org.nspasteboard.TransientType` 类型（nspasteboard.com 社区约定）。
//!     此检测需 AppKit 调用，将在具备 macOS 环境时补齐（见下方回退）。
//!
//! 非上述平台，或平台检测未实现时，一律返回 `false`（视为非敏感），保证功能
//! 可用且不误伤——宁可漏判也不阻断正常同步；密码类工具在 Windows 上有可靠标记。

/// 判断当前系统剪贴板内容是否被标记为敏感/瞬态（应跳过同步）。
///
/// 该函数读取的是"标记"而非内容本身，开销极小。在剪贴板变化后、决定是否
/// 广播前调用。
pub fn clipboard_is_sensitive() -> bool {
    platform::is_sensitive()
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::System::DataExchange::{
        IsClipboardFormatAvailable, RegisterClipboardFormatW,
    };

    /// 微软官方约定：存在此格式表示内容不应进入剪贴板历史/云剪贴板。
    /// 详见 Windows 剪贴板文档中的 "Excluding content"。
    const EXCLUDE_MONITOR: &str = "ExcludeClipboardContentFromMonitorProcessing";
    /// 部分应用使用此格式（值为 0 时排除历史）。存在即视为敏感的保守处理。
    const CAN_INCLUDE_HISTORY: &str = "CanIncludeInClipboardHistory";

    pub fn is_sensitive() -> bool {
        format_present(EXCLUDE_MONITOR) || format_present(CAN_INCLUDE_HISTORY)
    }

    /// 注册（或获取已注册的）自定义剪贴板格式 ID，并查询其当前是否在剪贴板上。
    ///
    /// `RegisterClipboardFormatW` 对同名格式返回同一稳定 ID，可安全重复调用。
    /// `IsClipboardFormatAvailable` 不需要打开剪贴板即可查询，开销极小。
    fn format_present(name: &str) -> bool {
        // 转成以 NUL 结尾的 UTF-16。
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: wide 是合法的、以 NUL 结尾的 UTF-16 缓冲；两个 API 均为纯查询，
        // 不获取所有权、不写入内存。
        unsafe {
            let id = RegisterClipboardFormatW(wide.as_ptr());
            if id == 0 {
                return false; // 注册失败：无法判断，保守视为非敏感。
            }
            IsClipboardFormatAvailable(id) != 0
        }
    }
}

#[cfg(not(windows))]
mod platform {
    // macOS 的 NSPasteboard ConcealedType/TransientType 检测将在具备 macOS
    // 编译环境时实现（objc2-app-kit 已作为 arboard 依赖存在）。当前回退为
    // 非敏感，保证跨平台可编译运行且不阻断同步。
    //
    // TODO(macos): 读取 NSPasteboard.general.types，若包含
    //   "org.nspasteboard.ConcealedType" 或 "org.nspasteboard.TransientType"
    //   则返回 true。
    pub fn is_sensitive() -> bool {
        false
    }
}
