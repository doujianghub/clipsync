//! Windows 平台的小工具。

use std::process::Command;

/// 让子进程不要弹出控制台窗口。
///
/// **为什么必须显式关掉**：本程序编译为 GUI 子系统（没有控制台），一旦启动
/// 一个控制台程序（`reg.exe`、`powershell.exe`），Windows 会**为它新建一个
/// 控制台窗口**——就是那个一闪而过的黑框。
///
/// 它出现的时机比想象中多：`autostart::is_enabled()` 会跑 `reg query`，
/// 而这个函数在**构建托盘菜单时**和**每次点完菜单刷新勾选状态时**都会被调用。
/// 于是程序一启动闪一下、之后每点一次菜单再闪一下。对一个"安静常驻"的
/// 后台工具，这是很扎眼的破绽。
pub fn hidden(cmd: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    /// `CREATE_NO_WINDOW`：不为控制台程序分配控制台窗口。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW)
}
