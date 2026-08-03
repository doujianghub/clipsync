//! 剪贴板变化的廉价探测令牌。
//!
//! 轮询监听需要频繁回答"剪贴板变了吗？"。直接读取并哈希内容开销较大
//! （尤其图片需解码 RGBA）。部分平台提供廉价的单调计数器，任何剪贴板
//! 变化都会使其递增，只需比较整数即可判断，契合"极低占用"目标。
//!
//!   - **Windows**：`GetClipboardSequenceNumber()` 返回会话级单调计数，
//!     无参数、无需打开剪贴板，开销极小。
//!   - **macOS**：`NSPasteboard.general.changeCount`，同样是单调递增整数，
//!     任何剪贴板变化都会使其 +1，读取无需打开剪贴板。
//!   - **其他平台**：暂无统一的廉价计数器，返回 `None`，由调用方回退到
//!     "读取内容并比较哈希"。

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

#[cfg(target_os = "macos")]
mod platform {
    use objc2_app_kit::NSPasteboard;

    pub fn change_token() -> Option<u64> {
        // changeCount 是纯读取，不打开剪贴板、不改变其状态，可在任意线程调用。
        let pb = NSPasteboard::generalPasteboard();
        // NSInteger（isize）单调递增。理论上不为负；用 as u64 保留全部位模式，
        // 令牌只做相等比较，不参与算术，故即便回绕也不影响正确性。
        Some(pb.changeCount() as u64)
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    // 其它平台暂无统一的廉价计数器，回退到内容哈希比较。
    pub fn change_token() -> Option<u64> {
        None
    }
}
