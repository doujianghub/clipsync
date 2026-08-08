//! Windows 的原生文本输入对话框。
//!
//! TaskDialog 覆盖了信息框、确认框、选择框，唯独**没有文本输入**。这里用
//! 裸 Win32 补上最后这一块，于是整套弹窗彻底不再需要 PowerShell 子进程。
//!
//! 换掉 PowerShell + WinForms 的理由，用户的截图说得比什么都清楚：
//!   - 背后杵着一个巨大的 PowerShell 黑窗；
//!   - 正文被输入框盖住一半，按钮挤成一团——WinForms 的布局是按 96dpi
//!     的绝对像素摆的，在高 DPI 屏上必然错位；
//!   - 每次弹窗要多花 100–300ms 起进程。
//!
//! 这里的布局按 **GetDpiForWindow 实际缩放**计算，字体取系统 UI 字体
//! （`SPI_GETNONCLIENTMETRICS` 的 `lfMessageFont`，也就是 Windows 自己
//! 对话框用的那个），所以在任何缩放下都和系统窗口一致。

use anyhow::{anyhow, Result};
use std::cell::RefCell;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, DrawTextW, GetDC, GetStockObject, ReleaseDC, SelectObject,
    COLOR_WINDOW, DEFAULT_GUI_FONT, DT_CALCRECT, DT_WORDBREAK, HFONT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// 控件 ID。
const ID_EDIT: isize = 100;
const ID_OK: isize = 1; // IDOK
const ID_CANCEL: isize = 2; // IDCANCEL

/// 96 dpi 下的基准尺寸，实际使用时按真实 DPI 缩放。
///
/// 正文高度**不在此列**——它按文本实测（见 `measure_text_height`）。写死高度
/// 正是上一版的毛病：提示一长就被输入框盖住，用户看不全该填什么。
const BASE_WIDTH: i32 = 400;
const MARGIN: i32 = 16;
const EDIT_H: i32 = 26;
const BTN_W: i32 = 84;
const BTN_H: i32 = 28;
const GAP: i32 = 10;

thread_local! {
    /// 用户点「确定」时取到的文本。窗口过程与调用方在同一线程，用
    /// thread_local 传值即可，不必把指针塞进窗口的 userdata。
    static RESULT: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 弹出输入框，返回用户输入（点取消或关闭返回 `None`）。
pub fn prompt(title: &str, body: &str) -> Result<Option<String>> {
    unsafe { prompt_inner(title, body) }
}

unsafe fn prompt_inner(title: &str, body: &str) -> Result<Option<String>> {
    let class = wide("ClipSyncInputDialog");
    let hinst = GetModuleHandleW(std::ptr::null());

    // 注册窗口类。重复注册会失败，忽略即可——同一进程里弹第二次时本就已注册。
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: std::ptr::null_mut(),
        hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
        hbrBackground: (COLOR_WINDOW + 1) as _,
        lpszMenuName: std::ptr::null(),
        lpszClassName: class.as_ptr(),
    };
    RegisterClassW(&wc);

    RESULT.with(|r| *r.borrow_mut() = None);

    let title_w = wide(title);
    // 先按 96dpi 建窗口，拿到 hwnd 后再按其真实 DPI 调整——窗口在哪个
    // 显示器上、那块屏幕缩放多少，只有窗口存在之后才知道。
    let hwnd = CreateWindowExW(
        WS_EX_DLGMODALFRAME | WS_EX_TOPMOST,
        class.as_ptr(),
        title_w.as_ptr(),
        WS_POPUPWINDOW | WS_CAPTION,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        BASE_WIDTH,
        200,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        hinst,
        std::ptr::null(),
    );
    if hwnd.is_null() {
        return Err(anyhow!("创建输入窗口失败"));
    }

    let dpi = GetDpiForWindow(hwnd).max(96);
    let s = |v: i32| v * dpi as i32 / 96; // 按真实缩放换算

    let font = ui_font(dpi);
    let body_w = wide(body);

    // 正文宽度定下来后，按真实字体量出它需要多高——写死高度会让长提示
    // 被下面的输入框盖住（上一版就是如此）。
    let text_w = s(BASE_WIDTH - MARGIN * 2);
    let label_h = measure_text_height(hwnd, font, &body_w, text_w).max(s(20));

    let label = CreateWindowExW(
        0,
        wide("STATIC").as_ptr(),
        body_w.as_ptr(),
        WS_CHILD | WS_VISIBLE,
        s(MARGIN),
        s(MARGIN),
        text_w,
        label_h,
        hwnd,
        std::ptr::null_mut(),
        hinst,
        std::ptr::null(),
    );
    let edit_y = s(MARGIN) + label_h + s(GAP);
    let edit = CreateWindowExW(
        WS_EX_CLIENTEDGE,
        wide("EDIT").as_ptr(),
        wide("").as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | (ES_AUTOHSCROLL as u32),
        s(MARGIN),
        edit_y,
        text_w,
        s(EDIT_H),
        hwnd,
        ID_EDIT as _,
        hinst,
        std::ptr::null(),
    );
    let btn_y = edit_y + s(EDIT_H) + s(GAP + 4);
    let ok = CreateWindowExW(
        0,
        wide("BUTTON").as_ptr(),
        wide("确定").as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | (BS_DEFPUSHBUTTON as u32),
        s(MARGIN) + text_w - s(BTN_W * 2 + GAP),
        btn_y,
        s(BTN_W),
        s(BTN_H),
        hwnd,
        ID_OK as _,
        hinst,
        std::ptr::null(),
    );
    let cancel = CreateWindowExW(
        0,
        wide("BUTTON").as_ptr(),
        wide("取消").as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP,
        s(MARGIN) + text_w - s(BTN_W),
        btn_y,
        s(BTN_W),
        s(BTN_H),
        hwnd,
        ID_CANCEL as _,
        hinst,
        std::ptr::null(),
    );

    // 系统 UI 字体，否则控件会用 Win3.1 时代的位图字体（那才是真的丑）。
    for c in [label, edit, ok, cancel] {
        SendMessageW(c, WM_SETFONT, font as WPARAM, 1);
    }

    // 按内容把窗口调到合适大小并居中。
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: s(BASE_WIDTH),
        bottom: btn_y + s(BTN_H + MARGIN),
    };
    AdjustWindowRectEx(
        &mut rect,
        WS_POPUPWINDOW | WS_CAPTION,
        0,
        WS_EX_DLGMODALFRAME,
    );
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    let sw = GetSystemMetrics(SM_CXSCREEN);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        (sw - w) / 2,
        (sh - h) / 2,
        w,
        h,
        SWP_SHOWWINDOW,
    );
    SetForegroundWindow(hwnd);
    SetFocus(edit);

    // 模态消息循环：跑到窗口销毁为止。
    let mut msg = MSG {
        hwnd: std::ptr::null_mut(),
        message: 0,
        wParam: 0,
        lParam: 0,
        time: 0,
        pt: POINT { x: 0, y: 0 },
    };
    // 用 PeekMessage 而非 GetMessage：窗口销毁后队列里可能再没有消息，
    // GetMessage 会一直阻塞，弹窗函数就永远不返回了。
    loop {
        if IsWindow(hwnd) == 0 {
            break;
        }
        if PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            if msg.message == WM_QUIT {
                break;
            }
            // IsDialogMessage 负责 Tab 切换、回车触发默认按钮、Esc 取消。
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        } else {
            // 队列空了就让出 CPU，别空转。
            WaitMessage();
        }
    }

    DeleteObject(font as _);
    Ok(RESULT.with(|r| r.borrow_mut().take()))
}

/// 在给定宽度下，量出这段文字自动换行后需要多高。
///
/// 用 `DT_CALCRECT | DT_WORDBREAK` 让 GDI 按**实际字体**算——这样无论用户
/// 把系统字号调多大、提示文案改多长，正文都不会被下面的控件盖住。
unsafe fn measure_text_height(hwnd: HWND, font: HFONT, text: &[u16], width: i32) -> i32 {
    let hdc = GetDC(hwnd);
    if hdc.is_null() {
        return width / 8; // 拿不到 DC 时给个不至于太离谱的估计
    }
    let old = SelectObject(hdc, font as _);
    let mut r = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: 0,
    };
    DrawTextW(hdc, text.as_ptr(), -1, &mut r, DT_CALCRECT | DT_WORDBREAK);
    SelectObject(hdc, old);
    ReleaseDC(hwnd, hdc);
    r.bottom - r.top
}

/// 取系统对话框字体。拿不到就退回 `DEFAULT_GUI_FONT`。
unsafe fn ui_font(dpi: u32) -> HFONT {
    let mut ncm: NONCLIENTMETRICSW = std::mem::zeroed();
    ncm.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
    let ok = SystemParametersInfoForDpi(
        SPI_GETNONCLIENTMETRICS,
        ncm.cbSize,
        &mut ncm as *mut _ as *mut _,
        0,
        dpi,
    );
    if ok != 0 {
        let f = CreateFontIndirectW(&ncm.lfMessageFont);
        if !f.is_null() {
            return f;
        }
    }
    GetStockObject(DEFAULT_GUI_FONT) as HFONT
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam & 0xFFFF) as isize;
            if id == ID_OK {
                let edit = GetDlgItem(hwnd, ID_EDIT as i32);
                let len = GetWindowTextLengthW(edit);
                let mut buf = vec![0u16; len as usize + 1];
                let n = GetWindowTextW(edit, buf.as_mut_ptr(), buf.len() as i32);
                let text = String::from_utf16_lossy(&buf[..n as usize]);
                RESULT.with(|r| *r.borrow_mut() = Some(text));
                DestroyWindow(hwnd);
                return 0;
            }
            if id == ID_CANCEL {
                DestroyWindow(hwnd);
                return 0;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            // **不要用 PostQuitMessage**：它往线程消息队列塞 WM_QUIT，
            // 而 WM_QUIT 会让**该线程的所有** GetMessage 循环退出。弹窗现在
            // 跑在后台线程，看似无害；可一旦哪天被挪到主线程调用，就会顺手
            // 把托盘的消息循环也终结掉——表现为点一下设置，整个程序静默退出。
            // 循环靠下面的 IsWindow 判断收尾，不需要 WM_QUIT。
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
