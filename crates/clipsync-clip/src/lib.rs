//! 剪贴板抽象层：读写系统剪贴板、监听变化、判定敏感内容。
//!
//! 本 crate 定义平台无关的 trait，具体后端在各里程碑填充：
//!   - M1：`arboard` 文本/图片 + 平台原生监听（Win 事件 / macOS changeCount）
//!         + 敏感内容探测。
//!   - M4：文件列表（Win CF_HDROP / macOS file-url）。
//!
//! 当前提供接口定义与一个内存 stub 后端（`stub` 模块），使上层可先行
//! 组装与测试，不阻塞在平台细节上。

use anyhow::Result;
use clipsync_core::ClipContent;

pub mod arboard_backend;
pub mod change_token;
pub mod filelist;
pub mod formats;
#[cfg(target_os = "macos")]
pub mod image_mac;
pub mod sensitive;
pub mod stub;

/// 系统剪贴板是**全局单例**，而 `cargo test` 默认并行跑测试。
///
/// 任何"写入剪贴板 → 读回来断言"的测试若同时运行，就会读到另一个测试刚写
/// 进去的内容，表现为随机失败——且失败信息指向被读的那个测试，与真正的
/// 肇事者无关，极难定位。所有触碰系统剪贴板的测试都必须先取这把锁。
///
/// 锁中毒（某个测试 panic）时取回内部值继续：一个测试失败不应把其余全部
/// 拖成连锁失败，那会掩盖真实的失败点。
///
/// 只在 macOS 上编译：眼下所有"写入再读回"的测试都是 macOS 限定的
/// （Windows 侧的剪贴板行为靠交叉检查与实机验证），在别的目标上留着它
/// 只会得到一条 dead_code 告警。
#[cfg(all(test, target_os = "macos"))]
pub(crate) fn clipboard_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub use arboard_backend::{ArboardClipboard, PollingWatcher};
pub use sensitive::clipboard_is_sensitive;

/// 一次剪贴板读取结果：内容 + 是否敏感 + （文件情形下的）本机路径。
#[derive(Debug, Clone)]
pub struct ClipRead {
    pub content: ClipContent,
    /// 平台层判定为敏感/瞬态（如密码管理器标记），上层据此跳过同步。
    pub sensitive: bool,
    /// 内容为文件时，各文件在本机的绝对路径（顺序与 `ClipContent::Files` 一致）。
    ///
    /// 两个用途：
    ///   1. 对端索取内容时，据此流式读取文件字节。
    ///   2. 识别"这是我们自己刚落地的接收文件"，避免把收到的文件又广播回去。
    pub file_paths: Vec<std::path::PathBuf>,
    /// 本次复制里被系统**拒绝读取**的文件，及该去哪儿开权限。
    ///
    /// 这不是错误——同一次复制里其它文件照常同步。但它必须能传到上层：
    /// 权限被拒是用户点两下就能解决的事，而它的默认表现是**什么都不发生**。
    /// 只记日志等于没说，托盘程序的用户不会去翻日志。
    pub denied: Vec<DeniedFile>,
}

/// 一个因权限读不了的文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeniedFile {
    pub path: std::path::PathBuf,
    /// 这是什么位置（如"另一个 App 的私有数据目录"）。
    pub reason: String,
    /// 系统设置里对应的那一页。
    pub where_to_fix: String,
}

impl ClipRead {
    /// 构造一个非文件的读取结果。
    pub fn simple(content: ClipContent, sensitive: bool) -> Self {
        Self {
            content,
            sensitive,
            file_paths: Vec::new(),
            denied: Vec::new(),
        }
    }

    /// 附上本次读取中被系统拒绝的文件。
    pub fn with_denied(mut self, denied: Vec<DeniedFile>) -> Self {
        self.denied = denied;
        self
    }
}

/// 剪贴板后端：读写系统剪贴板。
pub trait Clipboard: Send {
    /// 读取当前剪贴板内容。返回 `Ok(None)` 表示剪贴板为空或内容不支持。
    fn read(&mut self) -> Result<Option<ClipRead>>;

    /// 将内容写入系统剪贴板。
    ///
    /// 注意：调用方应在写入前通过引擎登记预期回声哈希（防回环）。
    fn write(&mut self, content: &ClipContent) -> Result<()>;
}

/// 剪贴板变化监听：当系统剪贴板发生变化时通过回调通知。
///
/// 平台实现：
///   - Windows：`AddClipboardFormatListener` + 隐藏消息窗口（事件驱动）。
///   - macOS：轮询 `NSPasteboard.changeCount`（仅比较整型，开销可忽略）。
pub trait ClipboardWatcher: Send {
    /// 阻塞运行监听循环，每次变化调用一次 `on_change`。
    ///
    /// `on_change` 返回 `false` 时退出循环（用于优雅关闭）。
    fn run(&mut self, on_change: &mut dyn FnMut() -> bool) -> Result<()>;
}
