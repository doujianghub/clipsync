//! 剪贴板变化的廉价探测令牌。
//!
//! 轮询监听需要频繁回答"剪贴板变了吗？"。直接读取并哈希内容开销较大
//! （尤其图片需解码 RGBA）。部分平台提供廉价的单调计数器，任何剪贴板
//! 变化都会使其递增，只需比较整数即可判断，契合"极低占用"目标。
//!
//!   - **Windows**：`GetClipboardSequenceNumber()` 返回会话级单调计数，
//!     无参数、无需打开剪贴板，开销极小。
//!   - **其他平台**：暂无统一的廉价计数器，返回 `None`，由调用方回退到
//!     "读取内容并比较哈希"。macOS 可在后续用 `NSPasteboard.changeCount`
//!     补充（objc2-app-kit 已作为依赖存在）。

/// 返回当前剪贴板的变化令牌。
///
/// `Some(n)`：单调令牌，值变化即代表剪贴板发生过变化。
/// `None`：本平台无廉价令牌，调用方应回退到内容哈希比较。
pub fn clipboard_change_token() -> Option<u64> {
    platform::change_token()
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;

    pub fn change_token() -> Option<u64> {
        // SAFETY: 无参纯查询，返回会话级序列号；始终安全调用。
        // 返回 0 表示当前无剪贴板序列（罕见），也作为一个合法令牌值处理。
        let seq = unsafe { GetClipboardSequenceNumber() };
        Some(seq as u64)
    }
}

#[cfg(not(windows))]
mod platform {
    // TODO(macos): 用 NSPasteboard.general.changeCount 提供廉价令牌。
    pub fn change_token() -> Option<u64> {
        None
    }
}
