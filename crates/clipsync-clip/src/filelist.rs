//! 剪贴板文件列表的读写（平台特定）。
//!
//! 剪贴板里的"文件"是**路径引用**而非内容：在资源管理器里复制文件，剪贴板存的
//! 只是路径，内容要到粘贴时才被读取。本模块负责在各平台上读出这些路径、以及把
//! 一组路径写回剪贴板，使对端粘贴时表现得与本机复制粘贴完全一致。
//!
//!   - **Windows**：`CF_HDROP` 格式。读用 `DragQueryFileW` 枚举；写需构造
//!     `DROPFILES` 头 + 双 NUL 结尾的宽字符路径表，放入全局内存交给系统。
//!   - **macOS**：`NSPasteboard` 的 `public.file-url`。读用
//!     `readObjectsForClasses:options:` 取 `NSURL`（须加 FileURLsOnly 选项，
//!     否则网页链接也会被当成文件）；写用 `writeObjects` 放入 `NSURL` 数组。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clipsync_core::FileMeta;

/// 读取剪贴板中的文件路径。`Ok(None)` 表示剪贴板里没有文件。
pub fn read_file_paths() -> Result<Option<Vec<PathBuf>>> {
    platform::read_file_paths()
}

/// 将一组文件路径写入剪贴板（使其可被其它程序正常粘贴）。
pub fn write_file_paths(paths: &[PathBuf]) -> Result<()> {
    platform::write_file_paths(paths)
}

/// 本平台是否支持文件列表读写。
pub fn supported() -> bool {
    platform::SUPPORTED
}

/// 这个错误是不是"系统不让读"。
///
/// 要顺着 `anyhow` 的因果链找 `io::Error`——上层加过 context，直接比对字符串
/// 会在措辞一改时失灵。
pub fn is_permission_denied(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

/// 系统拒绝读取某个路径时的解释。
///
/// **判据是路径形态而不是 errno**：macOS 的 TCC 一律返回
/// `Operation not permitted`，从错误码看不出是哪一类保护，而路径能看出来。
pub struct Denied {
    /// 一句话说清这是什么位置。
    pub reason: &'static str,
    /// 该去系统设置的哪一页把它打开。
    pub where_to_fix: &'static str,
}

/// 解释一个路径为什么可能被系统拒绝。
///
/// 只在确实拿到 `PermissionDenied` 之后调用——它不判断能不能读，只负责
/// 把"读不了"翻译成用户能照着做的一句话。
pub fn explain_denied(path: &Path) -> Denied {
    let p = path.to_string_lossy();
    let home = std::env::var("HOME").unwrap_or_default();
    let under = |sub: &str| !home.is_empty() && p.starts_with(&format!("{home}/{sub}"));

    if under("Library/Containers") || under("Library/Group Containers") {
        return Denied {
            reason: "这是另一个 App 的私有数据目录（微信、QQ 这类把收到的文件存在自己容器里）",
            // 实机截图确认过：这类授权出现在「文件与文件夹」里 ClipSync 名下，
            // 与「桌面」「下载」并列，条目名就是那个 App（如「微信」）。
            // 不是「App 管理」——那一项管的是"修改其它 App"，另一回事。
            where_to_fix: "系统设置 › 隐私与安全性 › 文件与文件夹 › ClipSync",
        };
    }
    for (dir, label) in [
        ("Desktop", "「桌面」文件夹"),
        ("Documents", "「文稿」文件夹"),
        ("Downloads", "「下载」文件夹"),
    ] {
        if under(dir) {
            return Denied {
                reason: label,
                where_to_fix: "系统设置 › 隐私与安全性 › 文件与文件夹",
            };
        }
    }
    if p.starts_with("/Volumes/") {
        return Denied {
            reason: "这是外置磁盘或网络卷",
            where_to_fix: "系统设置 › 隐私与安全性 › 可移除卷宗／网络卷宗",
        };
    }
    Denied {
        reason: "系统拒绝了访问，但路径不属于已知的几类受保护位置",
        where_to_fix: "系统设置 › 隐私与安全性 › 完全磁盘访问权限（最后的办法）",
    }
}

/// 剪贴板当前提供的全部数据类型标识，供排查用。
///
/// "复制了文件却没同步"有两种完全不同的成因：**根本没有文件类型**（有些
/// App 放的是"承诺文件"或纯文本），与**有路径但读不了**（权限）。光看
/// 同步结果分不出这两者，而这两者的解法南辕北辙——所以要能把原始类型列出来。
pub fn pasteboard_types() -> Vec<String> {
    platform::pasteboard_types()
}

/// 由路径生成 [`FileMeta`]：文件名 + 大小 + 传输标识。
///
/// 标识由 (文件名, 大小, 修改时间) 派生，**不读取文件内容**——复制 100MB 文件
/// 时不会产生任何磁盘读取延迟。文件被修改后标识自然改变，旧缓存随之失效。
pub fn meta_for_path(path: &Path) -> Result<FileMeta> {
    // 用 `context` 而不是 `anyhow!("...{e}")`：后者把 `io::Error` 格式化成字符串
    // 就丢了类型，调用方再也分不出"权限被拒"和"文件不存在"——而这两者对用户
    // 的意义完全不同，一个要去点系统设置，一个只是文件没了。
    let md =
        std::fs::metadata(path).with_context(|| format!("读取文件信息失败 {}", path.display()))?;

    // **stat 过了不等于读得了。**
    //
    // macOS 的 TCC 保护的是文件**内容**：`stat` 常常照样成功，`open` 才被拒。
    // （拿 `/etc/sudoers` 一试便知：metadata 返回 1709 字节，open 返回
    // PermissionDenied。）
    //
    // 少了这一步，从微信这类 App 复制文件会走进一条最难查的岔路：本机以为
    // 一切正常，把元数据广播出去、日志里写着"已同步"，对端收到后来索取内容，
    // 我们这才发现打不开，回一句 FileUnavailable——而对端只是默默丢弃，**它的
    // 剪贴板原封不动**。用户在那边一粘贴，出来的是上一次复制的东西。两边都
    // 没有任何提示，现象是"同步串台了"，谁也想不到根因是个权限开关。
    //
    // 代价是每个文件多一次 open/close（几微秒），换来的是在**复制的那一刻**
    // 就能把话说清楚。只对普通文件做：对 FIFO 之类 open 会阻塞。
    if md.is_file() {
        drop(std::fs::File::open(path).with_context(|| format!("打不开文件 {}", path.display()))?);
    }
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".to_string());
    let size = md.len();
    let mtime_nanos = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let mut h = clipsync_core::hash::Hasher::new();
    h.update(name.as_bytes());
    h.update(&size.to_le_bytes());
    h.update(&mtime_nanos.to_le_bytes());
    Ok(FileMeta::new(name, size, h.finish()))
}

#[cfg(windows)]
mod platform {
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;

    use anyhow::{anyhow, Result};
    use windows_sys::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::UI::Shell::DragQueryFileW;

    pub const SUPPORTED: bool = true;

    /// 剪贴板文件列表格式。
    const CF_HDROP: u32 = 15;

    /// `CF_HDROP` 数据的头部，其后紧跟双 NUL 结尾的宽字符路径表。
    #[repr(C)]
    struct DropFiles {
        /// 路径表相对本结构起始处的字节偏移（即本结构大小）。
        p_files: u32,
        pt_x: i32,
        pt_y: i32,
        /// 是否为非客户区坐标。
        f_nc: i32,
        /// 路径表是否为宽字符（我们始终用 UTF-16，故为 TRUE）。
        f_wide: i32,
    }

    /// 打开剪贴板的 RAII 守卫，确保任何返回路径都会关闭剪贴板。
    ///
    /// 剪贴板是系统独占资源，忘记关闭会让其它程序无法复制粘贴——这正是我们
    /// 要极力避免的干扰。
    struct ClipboardGuard;

    impl ClipboardGuard {
        fn open() -> Result<Self> {
            // SAFETY: 传入空窗口句柄表示关联当前任务，是文档允许的用法。
            let ok = unsafe { OpenClipboard(std::ptr::null_mut()) };
            if ok == 0 {
                return Err(anyhow!("打开剪贴板失败（可能被其它程序占用）"));
            }
            Ok(Self)
        }
    }

    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            // SAFETY: 与 OpenClipboard 成对调用。
            unsafe { CloseClipboard() };
        }
    }

    /// Windows 的剪贴板格式是数字 ID，没有 macOS 那样的字符串标识。
    /// 只报告与本模块相关的那一个是否在场——排查文件同步足够了。
    pub fn pasteboard_types() -> Vec<String> {
        // SAFETY: 纯查询调用。
        if unsafe { IsClipboardFormatAvailable(CF_HDROP) } != 0 {
            vec!["CF_HDROP".to_string()]
        } else {
            Vec::new()
        }
    }

    pub fn read_file_paths() -> Result<Option<Vec<PathBuf>>> {
        // 先用无需打开剪贴板的廉价查询判断，避免无谓地与其它程序抢锁。
        // SAFETY: 纯查询调用。
        if unsafe { IsClipboardFormatAvailable(CF_HDROP) } == 0 {
            return Ok(None);
        }

        let _guard = ClipboardGuard::open()?;
        // SAFETY: 已成功打开剪贴板；返回的句柄由系统拥有，我们只读不释放。
        let handle: HANDLE = unsafe { GetClipboardData(CF_HDROP) };
        if handle.is_null() {
            return Ok(None);
        }

        // SAFETY: handle 为 CF_HDROP 数据，可作 HDROP 使用。传 u32::MAX 表示
        // 查询文件数量而非某个具体文件。
        let count = unsafe { DragQueryFileW(handle, u32::MAX, std::ptr::null_mut(), 0) };
        if count == 0 {
            return Ok(None);
        }

        let mut paths = Vec::with_capacity(count as usize);
        for i in 0..count {
            // 先查所需长度（不含结尾 NUL），再取内容。
            // SAFETY: i < count，缓冲区长度与传入的 cch 一致。
            let len = unsafe { DragQueryFileW(handle, i, std::ptr::null_mut(), 0) };
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let written = unsafe { DragQueryFileW(handle, i, buf.as_mut_ptr(), buf.len() as u32) };
            if written == 0 {
                continue;
            }
            buf.truncate(written as usize);
            paths.push(PathBuf::from(OsString::from_wide(&buf)));
        }

        if paths.is_empty() {
            Ok(None)
        } else {
            Ok(Some(paths))
        }
    }

    pub fn write_file_paths(paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }

        // 构造路径表：每条路径以 NUL 结尾，整体再以一个额外 NUL 收尾。
        let mut list: Vec<u16> = Vec::new();
        for p in paths {
            list.extend(OsStr::new(p).encode_wide());
            list.push(0);
        }
        list.push(0);

        let header_size = std::mem::size_of::<DropFiles>();
        let total = header_size + list.len() * std::mem::size_of::<u16>();

        // SAFETY: 分配可移动全局内存；成功后由我们填充，随后所有权移交系统。
        let hglobal: HGLOBAL = unsafe { GlobalAlloc(GMEM_MOVEABLE, total) };
        if hglobal.is_null() {
            return Err(anyhow!("为剪贴板文件列表分配内存失败"));
        }

        // SAFETY: 锁定刚分配的内存以写入；随后解锁。
        unsafe {
            let ptr = GlobalLock(hglobal);
            if ptr.is_null() {
                return Err(anyhow!("锁定剪贴板内存失败"));
            }
            // 写头部。
            let header = ptr as *mut DropFiles;
            std::ptr::write(
                header,
                DropFiles {
                    p_files: header_size as u32,
                    pt_x: 0,
                    pt_y: 0,
                    f_nc: 0,
                    f_wide: 1, // 使用宽字符路径
                },
            );
            // 紧随其后写路径表。
            let list_ptr = (ptr as *mut u8).add(header_size) as *mut u16;
            std::ptr::copy_nonoverlapping(list.as_ptr(), list_ptr, list.len());
            GlobalUnlock(hglobal);
        }

        let _guard = ClipboardGuard::open()?;
        // SAFETY: 已打开剪贴板。清空后放入我们的数据。
        unsafe {
            EmptyClipboard();
            let prev = SetClipboardData(CF_HDROP, hglobal as HANDLE);
            // SetClipboardData 成功后内存归系统所有，不可再由我们释放。
            // 返回空句柄表示失败——此时内存仍归我们，但让它随进程回收即可，
            // 贸然释放反而有二次释放风险。
            if prev.is_null() && hglobal.is_null() {
                return Err(anyhow!("写入剪贴板文件列表失败"));
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::path::PathBuf;

    use anyhow::{anyhow, Result};
    use objc2::runtime::AnyObject;
    use objc2::ClassType;
    use objc2_app_kit::{NSPasteboard, NSPasteboardURLReadingFileURLsOnlyKey};
    use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};

    pub const SUPPORTED: bool = true;

    /// Finder 复制文件时放入剪贴板的类型。仅用于"是否有文件"的廉价预判。
    const FILE_URL: &str = "public.file-url";

    pub fn pasteboard_types() -> Vec<String> {
        NSPasteboard::generalPasteboard()
            .types()
            .map(|t| t.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default()
    }

    pub fn read_file_paths() -> Result<Option<Vec<PathBuf>>> {
        let pb = NSPasteboard::generalPasteboard();

        // 先用类型列表廉价预判，没有文件就不必构造读取参数。
        match pb.types() {
            Some(types) => {
                if !types.iter().any(|t| t.to_string() == FILE_URL) {
                    return Ok(None);
                }
            }
            None => return Ok(None), // 剪贴板为空
        }

        // 只要文件 URL：不加此选项时，网页里复制的普通链接也会被当成文件读出来。
        let only_files = NSNumber::new_bool(true);
        // SAFETY: AppKit 导出的常量键，读取始终有效。
        let key = unsafe { NSPasteboardURLReadingFileURLsOnlyKey };
        let options: objc2::rc::Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_slices(&[key], &[&*only_files as &AnyObject]);
        let classes = NSArray::from_slice(&[NSURL::class()]);

        // SAFETY: classes 只含 NSURL 类对象，options 的键值类型与 AppKit 约定一致，
        // 满足 readObjectsForClasses:options: 的两项要求。
        let objects = unsafe { pb.readObjectsForClasses_options(&classes, Some(&options)) };
        let Some(objects) = objects else {
            return Ok(None);
        };

        let mut paths = Vec::with_capacity(objects.len());
        for obj in objects.iter() {
            // 依据 classes 参数，返回的元素必为 NSURL。
            // SAFETY: 元素类型由上面的 class_array 限定为 NSURL。
            let url: &NSURL = unsafe { &*(&*obj as *const AnyObject as *const NSURL) };
            // 非文件 URL 会返回 None；已用 FileURLsOnly 过滤，这里再兜一层。
            if let Some(p) = url.path() {
                // NSURL 往返会把文件名规范化成 NFD（`é` 由单码位 U+00E9 拆成
                // `e` + U+0301），而磁盘上、Windows 上都是 NFC。若原样使用，
                // 同一个文件在"直接读磁盘"与"经剪贴板读取"两条路径下会算出
                // 不同的 id（id 由文件名派生），导致跨平台缓存命中与断点续传
                // 对含重音字符的文件静默失效——不报错，只是每次都重传。
                // 这里归一化回 NFC，与磁盘和 Windows 侧保持一致。
                paths.push(PathBuf::from(
                    p.precomposedStringWithCanonicalMapping().to_string(),
                ));
            }
        }

        if paths.is_empty() {
            Ok(None)
        } else {
            Ok(Some(paths))
        }
    }

    pub fn write_file_paths(paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }

        // NSURL 需要持有到 writeObjects 调用结束，故先收集再借用。
        let urls: Vec<_> = paths
            .iter()
            .map(|p| {
                let s = NSString::from_str(&p.to_string_lossy());
                NSURL::fileURLWithPath(&s)
            })
            .collect();
        let refs: Vec<_> = urls
            .iter()
            .map(|u| objc2::runtime::ProtocolObject::from_ref(&**u))
            .collect();
        let array = NSArray::from_slice(&refs);

        let pb = NSPasteboard::generalPasteboard();
        // 必须先清空：writeObjects 只追加，残留的旧类型会让粘贴方读到过期内容。
        pb.clearContents();
        if !pb.writeObjects(&array) {
            return Err(anyhow!("写入剪贴板文件列表失败"));
        }

        // writeObjects 向 pasteboard 服务的提交是异步的：进程若在提交完成前退出，
        // 除第一条以外的条目会来不及落地——多文件同步会静默只剩一个文件。
        // 回读一次类型列表可强制完成这次往返。注意必须真的取数据：
        // 只数 pasteboardItems 的条数由本地应答，起不到同步作用。
        let _ = pb.types();

        // 确认条目数与预期一致，避免"写成功但内容不全"被静默接受。
        let written = pb.pasteboardItems().map(|i| i.len()).unwrap_or(0);
        if written < paths.len() {
            return Err(anyhow!(
                "写入剪贴板文件列表不完整：期望 {} 条，实际 {written} 条",
                paths.len()
            ));
        }
        Ok(())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use std::path::PathBuf;

    use anyhow::Result;

    // 其它平台暂不支持文件列表，文本与图片同步不受影响。
    pub const SUPPORTED: bool = false;

    pub fn pasteboard_types() -> Vec<String> {
        Vec::new()
    }

    pub fn read_file_paths() -> Result<Option<Vec<PathBuf>>> {
        Ok(None)
    }

    pub fn write_file_paths(_paths: &[PathBuf]) -> Result<()> {
        anyhow::bail!("本平台的文件列表写入尚未实现")
    }
}

#[cfg(test)]
#[path = "filelist_tests.rs"]
mod tests;
