//! Windows 的原生对话框（`TaskDialogIndirect`）。
//!
//! 这是 Vista 起系统自带的对话框，Windows 自己的确认框、更新提示用的都是它。
//! 相比早先"起 PowerShell 子进程画 WinForms 窗口"的做法，它一次解决四件事：
//!
//!   - **能用**：`powershell -Command -`（从 stdin 喂脚本）是无文件攻击的
//!     典型模式，杀软有充分理由拦下来——实测在 Windows 上所有弹窗都不出现，
//!     连「解除配对」都做不了。直接调 API 没有这个由头。
//!   - **不闪黑框**：GUI 子系统的进程创建控制台子进程时，Windows 会分配一个
//!     控制台窗口，一闪而过但很扎眼。这里不起任何子进程。
//!   - **好看**：系统渲染，自动跟随当前 Windows 版本的外观、深色模式、
//!     DPI 缩放与系统字体，不必自己声明 DPI 感知、自己挑字体。
//!   - **快**：省掉每次 100–300ms 的进程启动与几十 MB 内存。
//!
//! **唯一做不了的是文本输入**——TaskDialog 没有输入框。那条路径仍走
//! `dialog_win` 的 WinForms 实现（见该模块说明），好在只剩自定义数值这类
//! 低频操作用得上。

use anyhow::{anyhow, Result};
use windows_sys::core::{BOOL, HRESULT, PCWSTR};
use windows_sys::Win32::UI::Controls::{
    TASKDIALOGCONFIG, TASKDIALOG_BUTTON, TDF_ALLOW_DIALOG_CANCELLATION,
    TDF_POSITION_RELATIVE_TO_WINDOW, TDF_USE_COMMAND_LINKS,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{IDCANCEL, IDNO, IDYES};

/// `TaskDialogIndirect` 的签名，供运行时取址后调用。
type TaskDialogIndirectFn = unsafe extern "system" fn(
    *const TASKDIALOGCONFIG,
    *mut i32,
    *mut i32,
    *mut BOOL,
) -> HRESULT;

/// 运行时解析 `TaskDialogIndirect`，**绝不静态链接**。
///
/// 这个函数只存在于 **comctl32 v6**，而 Windows 默认加载给进程的是
/// `System32\comctl32.dll`（5.82 兼容版），里面没有它。静态导入的后果不是
/// "功能不可用"，而是**程序根本起不来**——加载器在启动时解析不到符号就直接
/// 报「无法定位程序输入点」。为一个锦上添花的对话框赔上整个程序的启动，
/// 完全不划算。
///
/// v6 要靠应用清单声明才能拿到（见 `clipsync.manifest`）。清单万一没生效，
/// 这里取址失败，调用方回退到 PowerShell 弹窗——程序照常运行。
///
/// 解析一次即缓存：失败也缓存，免得每次弹窗都白试一遍。
fn task_dialog_fn() -> Option<TaskDialogIndirectFn> {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    static RESOLVED: OnceLock<Option<usize>> = OnceLock::new();
    let addr = *RESOLVED.get_or_init(|| {
        let name: Vec<u16> = "comctl32.dll\0".encode_utf16().collect();
        // SAFETY: 名字是以 NUL 结尾的 UTF-16；失败返回空句柄。
        let lib = unsafe { LoadLibraryW(name.as_ptr()) };
        if lib.is_null() {
            return None;
        }
        // SAFETY: lib 有效；符号名是以 NUL 结尾的 ASCII。
        let proc = unsafe { GetProcAddress(lib, c"TaskDialogIndirect".as_ptr() as *const u8) };
        proc.map(|f| f as usize)
    });
    // SAFETY: 取到的地址确实是 TaskDialogIndirect，签名与上面的类型一致。
    addr.map(|a| unsafe { std::mem::transmute::<usize, TaskDialogIndirectFn>(a) })
}

/// 自定义按钮的起始 ID。
///
/// 从 1000 开始是为了避开 `IDOK`(1)、`IDCANCEL`(2) 这些系统预定义值——
/// 撞上的话就分不清用户点的是自定义按钮还是「取消」。
const FIRST_BUTTON_ID: i32 = 1000;

/// 把 Rust 字符串转成以 NUL 结尾的 UTF-16。
///
/// 返回的 `Vec` **必须在调用期间保持存活**：结构体里存的是裸指针，Vec 一旦
/// 提前析构，TaskDialog 读到的就是野指针。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn pcwstr(v: &[u16]) -> PCWSTR {
    v.as_ptr()
}

/// 弹一个只有「确定」的信息框。
pub fn show(title: &str, body: &str) -> Result<()> {
    let (title_w, body_w) = (wide(title), wide(body));
    let mut cfg = base_config();
    cfg.pszWindowTitle = pcwstr(&title_w);
    cfg.pszContent = pcwstr(&body_w);
    // TDCBF_OK_BUTTON = 1
    cfg.dwCommonButtons = 1;
    run(&cfg).map(|_| ())
}

/// 确认框。返回用户是否点了「是」。
///
/// 默认按钮设为「否」——用它的都是破坏性操作（解除配对），手快连按回车
/// 不该把事情做了。
pub fn confirm(title: &str, body: &str) -> Result<bool> {
    let (title_w, body_w) = (wide(title), wide(body));
    let mut cfg = base_config();
    cfg.pszWindowTitle = pcwstr(&title_w);
    cfg.pszContent = pcwstr(&body_w);
    // TDCBF_YES_BUTTON | TDCBF_NO_BUTTON = 2 | 4
    cfg.dwCommonButtons = 2 | 4;
    cfg.nDefaultButton = IDNO as i32;
    Ok(run(&cfg)? == IDYES as i32)
}

/// 列表选择，用 Command Links 呈现——每个选项是一个大按钮，比列表框好按也好看。
///
/// 返回选中项的下标；用户取消返回 `None`。
pub fn choose(title: &str, body: &str, items: &[String]) -> Result<Option<usize>> {
    if items.is_empty() {
        return Ok(None);
    }
    let (title_w, body_w) = (wide(title), wide(body));
    // 每个按钮的文本都要活到调用结束，先整体收好再取指针。
    let labels: Vec<Vec<u16>> = items.iter().map(|s| wide(s)).collect();
    let buttons: Vec<TASKDIALOG_BUTTON> = labels
        .iter()
        .enumerate()
        .map(|(i, l)| TASKDIALOG_BUTTON {
            nButtonID: FIRST_BUTTON_ID + i as i32,
            pszButtonText: pcwstr(l),
        })
        .collect();

    let mut cfg = base_config();
    cfg.pszWindowTitle = pcwstr(&title_w);
    cfg.pszContent = pcwstr(&body_w);
    cfg.dwFlags |= TDF_USE_COMMAND_LINKS;
    cfg.cButtons = buttons.len() as u32;
    cfg.pButtons = buttons.as_ptr();
    // TDCBF_CANCEL_BUTTON = 8：留一个明确的退出口。
    cfg.dwCommonButtons = 8;

    let pressed = run(&cfg)?;
    if pressed == IDCANCEL as i32 {
        return Ok(None);
    }
    let idx = pressed - FIRST_BUTTON_ID;
    Ok(usize::try_from(idx).ok().filter(|i| *i < items.len()))
}

fn base_config() -> TASKDIALOGCONFIG {
    let mut cfg = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        ..Default::default()
    };
    // 允许 Esc / 右上角关闭。没有它，只给 Command Links 的对话框会关不掉。
    cfg.dwFlags = TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW;
    cfg
}

/// 真正调用系统 API，返回被按下的按钮 ID。
fn run(cfg: &TASKDIALOGCONFIG) -> Result<i32> {
    let f = task_dialog_fn()
        .ok_or_else(|| anyhow!("comctl32 未提供 TaskDialogIndirect（缺少 v6 清单）"))?;
    let mut pressed: i32 = 0;
    // SAFETY: cfg 内的所有字符串指针都指向调用方仍持有的 Vec；
    // pnRadioButton / pfVerificationFlagChecked 传空表示不使用这两项功能。
    let hr = unsafe { f(cfg, &mut pressed, std::ptr::null_mut(), std::ptr::null_mut()) };
    if hr < 0 {
        return Err(anyhow!("TaskDialog 调用失败（HRESULT 0x{hr:08X}）"));
    }
    Ok(pressed)
}
