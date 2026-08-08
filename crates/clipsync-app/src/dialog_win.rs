//! Windows 的弹窗实现，**全部走原生 Win32，不起任何子进程**。
//!
//! 三类对话框各有归属：
//!   - 信息框 / 确认框 / 选择框 → `TaskDialogIndirect`（系统原生现代对话框）
//!   - 文本输入框 → 自绘 Win32 窗口（TaskDialog 唯独没有输入框）
//!
//! **为什么彻底删掉了 PowerShell 实现**：它曾作为回退保留，但实测下来那条
//! 路本身就是故障源，留着有害无益——
//!   - 弹窗背后杵着一个巨大的 PowerShell 黑窗；
//!   - WinForms 按 96dpi 绝对像素布局，高 DPI 屏上正文被输入框盖住、按钮
//!     挤成一团；
//!   - 每次弹窗多花 100–300ms 起进程；
//!   - 脚本传递本身就踩过两轮坑（stdin 编码按 GBK 解、`-Command -` 收不到
//!     脚本），排查代价极高，因为子进程的错误在主程序里看不见。
//!
//! 真正需要兜底的是"TaskDialog 拿不到"（缺 comctl32 v6）——那种情况下
//! 回退到 PowerShell 同样弹不出好窗口，不如让调用方按失败处理并记日志。

use anyhow::Result;

#[path = "dialog_win_task.rs"]
mod task;
#[path = "dialog_win_input.rs"]
mod input;

pub fn show(title: &str, body: &str) -> Result<()> {
    task::show(title, body)
}

pub fn ask_action(title: &str, body: &str, action: &str) -> Result<bool> {
    task::ask_action(title, body, action)
}

pub fn confirm(title: &str, body: &str) -> Result<bool> {
    task::confirm(title, body)
}

pub fn choose(title: &str, body: &str, items: &[String]) -> Result<Option<usize>> {
    task::choose(title, body, items)
}

pub fn prompt(title: &str, body: &str) -> Result<Option<String>> {
    input::prompt(title, body)
}
