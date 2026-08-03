//! 剪贴板文件列表的读写（平台特定）。
//!
//! 剪贴板里的"文件"是**路径引用**而非内容：在资源管理器里复制文件，剪贴板存的
//! 只是路径，内容要到粘贴时才被读取。本模块负责在各平台上读出这些路径、以及把
//! 一组路径写回剪贴板，使对端粘贴时表现得与本机复制粘贴完全一致。
//!
//!   - **Windows**：`CF_HDROP` 格式。读用 `DragQueryFileW` 枚举；写需构造
//!     `DROPFILES` 头 + 双 NUL 结尾的宽字符路径表，放入全局内存交给系统。
//!   - **macOS**：`NSPasteboard` 的 `public.file-url`（待具备 macOS 环境时补齐；
//!     当前返回"无文件"，文本与图片同步不受影响）。

use std::path::{Path, PathBuf};

use anyhow::Result;
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

/// 由路径生成 [`FileMeta`]：文件名 + 大小 + 传输标识。
///
/// 标识由 (文件名, 大小, 修改时间) 派生，**不读取文件内容**——复制 100MB 文件
/// 时不会产生任何磁盘读取延迟。文件被修改后标识自然改变，旧缓存随之失效。
pub fn meta_for_path(path: &Path) -> Result<FileMeta> {
    let md = std::fs::metadata(path)
        .map_err(|e| anyhow::anyhow!("读取文件信息失败 {}: {e}", path.display()))?;
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
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
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
            let written =
                unsafe { DragQueryFileW(handle, i, buf.as_mut_ptr(), buf.len() as u32) };
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

#[cfg(not(windows))]
mod platform {
    use std::path::PathBuf;

    use anyhow::Result;

    // macOS 的 NSPasteboard public.file-url 读写待具备编译环境时实现。
    // 当前返回"无文件"，不影响文本与图片同步。
    pub const SUPPORTED: bool = false;

    pub fn read_file_paths() -> Result<Option<Vec<PathBuf>>> {
        Ok(None)
    }

    pub fn write_file_paths(_paths: &[PathBuf]) -> Result<()> {
        anyhow::bail!("本平台的文件列表写入尚未实现")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_reflects_name_and_size() {
        let dir = std::env::temp_dir().join("clipsync_meta_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hello.txt");
        std::fs::write(&path, b"hello world").unwrap();

        let meta = meta_for_path(&path).unwrap();
        assert_eq!(meta.name, "hello.txt");
        assert_eq!(meta.size, 11);
        assert_ne!(meta.id, 0);
    }

    /// 同一文件（未改动）两次取元数据应得到相同标识——这是缓存命中的前提。
    #[test]
    fn meta_id_is_stable_for_unchanged_file() {
        let dir = std::env::temp_dir().join("clipsync_meta_test2");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stable.bin");
        std::fs::write(&path, vec![7u8; 64]).unwrap();

        let a = meta_for_path(&path).unwrap();
        let b = meta_for_path(&path).unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(a, b);
    }

    /// 内容改变（大小随之改变）应产生不同标识，使旧缓存失效。
    #[test]
    fn meta_id_changes_when_file_changes() {
        let dir = std::env::temp_dir().join("clipsync_meta_test3");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("changing.bin");

        std::fs::write(&path, b"first").unwrap();
        let a = meta_for_path(&path).unwrap();

        std::fs::write(&path, b"second-and-longer").unwrap();
        let b = meta_for_path(&path).unwrap();

        assert_ne!(a.id, b.id, "文件改动后标识必须变化，否则会用到脏缓存");
    }

    #[test]
    fn missing_file_reports_error() {
        let path = std::env::temp_dir().join("clipsync_definitely_missing_12345.bin");
        assert!(meta_for_path(&path).is_err());
    }
}
