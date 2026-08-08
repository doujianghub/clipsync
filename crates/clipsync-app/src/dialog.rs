//! 跨平台的简易提示框。
//!
//! 用途：程序从托盘启动时**没有终端**，`println!` 的内容用户根本看不到。
//! 配对码就属于这种情况——不弹窗等于功能不可用。
//!
//! 实现选择：调用系统自带的脚本宿主（macOS `osascript`、Windows PowerShell）
//! 起一个子进程弹窗，而不是用 `NSAlert`/`MessageBox` 原生 API。原因：
//!   - 原生弹窗要求在**主线程**调用，而配对跑在后台线程，需要把请求回抄到
//!     托盘循环里代为执行，改动绕且容易出错；
//!   - 子进程方式无需引入任何新依赖，两个平台写法对称。
//!
//! 代价是多一次进程启动（约 100ms）与外观朴素，对"偶尔弹一次"的场景可接受。
//!
//! **安全**：用户内容（设备名来自对端，属不可信输入）一律作为**参数**传给
//! 脚本，脚本体是固定字面量。实测确认字符串拼接方式可被恶意设备名注入并执行
//! 任意命令，故不采用拼接 + 转义的做法。

/// 弹出一个信息框。
///
/// 失败（无图形会话、脚本宿主缺失等）只记日志，绝不影响调用方流程——
/// 弹不出窗最多是体验退化，不该让配对或同步失败。
pub fn show_info(title: &str, body: &str) {
    if let Err(e) = platform::show(title, body) {
        tracing::warn!("弹窗失败（内容仍见于日志/终端）: {e:#}");
    }
}

#[cfg(target_os = "macos")]
#[path = "dialog_mac.rs"]
mod platform;
#[cfg(windows)]
#[path = "dialog_win.rs"]
mod platform;
#[cfg(not(any(target_os = "macos", windows)))]
#[path = "dialog_other.rs"]
mod platform;

/// 弹出一个确认框，返回用户是否确认。
///
/// 弹不出窗时返回 `false`——**破坏性操作在没法征得同意时必须当作"不要做"**，
/// 而不是默认执行。
pub fn confirm(title: &str, body: &str) -> bool {
    match platform::confirm(title, body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("确认框弹出失败，按「取消」处理: {e:#}");
            false
        }
    }
}

/// 信息框 + 一个动作按钮，返回用户是否按了那个动作。
///
/// 与 [`confirm`] 的区别在语气与默认值：这里正文本身是要给人看的内容
/// （配对码），动作按钮是顺手提供的一条岔路，默认按钮仍是「好」。confirm 是
/// "你确定要毁掉它吗"，默认落在取消。
///
/// 弹不出窗时返回 `false`——没能显示按钮就等于没人按过它。
pub fn ask_action(title: &str, body: &str, action: &str) -> bool {
    match platform::ask_action(title, body, action) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("弹窗失败（内容仍见于日志/终端）: {e:#}");
            false
        }
    }
}

/// 系统拒绝读取文件时，**当场**告诉用户一次。
///
/// **为什么非说不可**：这类失败的默认表现是"什么都没发生"，甚至更坏——本机
/// 以为同步成功了，对端却因为拿不到内容而保持原样，用户在那边一粘贴出来的是
/// 上一次复制的东西。两边都没提示，现象像是"同步串台了"，谁也想不到根因是
/// 一个权限开关。只记日志等于没说：托盘程序的用户不会去翻日志。
///
/// **为什么只说一次**：授权是一次性的事，说清就够了。用户若选择不理会（比如
/// 他就是不想让程序读那个目录），此后每复制一次都弹一遍才真是骚扰。重启程序
/// 后可以再说一次——那多半意味着他改过设置、正在重新试。
///
/// 弹窗会阻塞到用户点掉，所以一律另起线程：调用方多半是剪贴板监听线程或收发
/// 泵，占住任何一个都会让同步整体停摆，比原本的问题严重得多。
pub fn permission_hint_once(what: &std::path::Path, reason: &str, where_to_fix: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static TOLD: AtomicBool = AtomicBool::new(false);

    if TOLD.swap(true, Ordering::Relaxed) {
        return;
    }
    let name = what
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| what.display().to_string());
    let body = format!(
        "系统不允许 ClipSync 读取「{name}」，这次复制没能同步过去。\n\n\
         {reason}。\n\n\
         去「{where_to_fix}」里把 ClipSync 打开即可。\n\n\
         这句提示每次运行只出现一次；用 `clipsync clipdiag` 可逐条查看。"
    );
    std::thread::spawn(move || show_info("ClipSync 无法读取文件", &body));
}

/// 弹出一个列表让用户点选，返回选中项的下标。
///
/// **为什么设置项优先用它而不是输入框**：选常用值这件事，点一下本来就比
/// 打字省事。菜单里只留一行（`自动取回：100 MiB…`）保持清爽，把选择放进
/// 点击后的对话框——两头都不牺牲。
///
/// 列表末尾通常留一个「自定义…」，选中它再走 [`prompt`]。
///
/// `None` 表示用户取消或弹不出窗。
pub fn choose(title: &str, body: &str, items: &[String]) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    match platform::choose(title, body, items) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("选择框弹出失败: {e:#}");
            None
        }
    }
}

/// 弹出一个单行输入框，返回用户输入。
///
/// `None` 表示用户取消、或弹不出窗（无图形会话、脚本宿主缺失）。两者都按
/// "用户没给输入"处理——调用方本就无事可做。
///
/// 与 [`show_info`] 同样的注入防护：提示文字经参数/环境变量传入，脚本体是
/// 固定字面量。
pub fn prompt(title: &str, body: &str) -> Option<String> {
    match platform::prompt(title, body) {
        Ok(v) => v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        Err(e) => {
            tracing::warn!("输入框弹出失败: {e:#}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    /// 恶意"设备名"：把两个平台的脚本注入手法都塞进去。
    ///
    /// 挑选依据是"若真被解析会留下可观测痕迹"，而不是随便找些特殊字符：
    ///   - `$(...)` / 反引号：PowerShell 与 shell 的命令替换。写成新建文件，
    ///     一旦执行就会在 TEMP 留下 `clipsync_inject_marker.txt`。
    ///   - `"` `;` `&`：闭合引号、拼接下一条命令。
    ///   - `$env:USERNAME` / `%USERNAME%`：变量展开。若窗口里显示的是真实
    ///     用户名而非这串字面量，说明内容被当成了脚本文本。
    pub const EVIL_NAME: &str = concat!(
        r#"设备"A" ; $(New-Item -Path $env:TEMP\clipsync_inject_marker.txt -Force) "#,
        r#"& `whoami` $env:USERNAME %USERNAME% 结束"#
    );

    /// 手动目视验证：弹出配对码输入框。
    ///
    /// 跑法：`cargo test -p clipsync-app --bin clipsync -- --ignored manual_prompt`
    ///
    /// 判据：
    ///   1. 窗口出现，标题为「ClipSync 配对」，提示文字完整可读；
    ///   2. **Windows 上重点看字体**——正文应是 Segoe UI、边缘锐利，不该有
    ///      拉伸导致的毛边（那说明 DPI 声明没生效）；输入框应是等宽字体；
    ///   3. 输入内容点确定 → 打印 `RESULT=Some("...")`，值与所输一致；
    ///   4. 点取消 / 按 Esc → 打印 `RESULT=None`，且不报错。
    /// 手动目视验证：弹出设置项的选择列表。
    ///
    /// 跑法：`cargo test -p clipsync-app --bin clipsync -- --ignored manual_choose`
    ///
    /// 判据：
    ///   1. 是一个**干净的列表**，没有那个看着像"要输文件路径"的大图标
    ///      （`display dialog` 在子进程里显示的是 osascript 自己的脚本图标）；
    ///   2. 首项默认选中，双击或「确定」都能选定；
    ///   3. 点「取消」返回 `None`。
    #[test]
    #[ignore = "会弹窗并阻塞，需人工/脚本关闭"]
    fn manual_choose_dialog() {
        let items: Vec<String> = ["10 MiB", "100 MiB（默认）", "500 MiB", "自定义…"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let r = super::choose("自动取回上限", "当前：100 MiB", &items);
        println!("RESULT={r:?}");
    }

    #[test]
    #[ignore = "会弹窗并阻塞，需人工/脚本关闭"]
    fn manual_prompt_dialog() {
        let r = super::prompt(
            "ClipSync 配对",
            "请输入对方显示的配对码：\n（在对方设备的托盘菜单里选「显示配对码…」）",
        );
        println!("RESULT={r:?}");
    }

    /// 手动目视验证：弹出一个标题与正文都含注入载荷的窗口。
    ///
    /// 默认 `#[ignore]`——它会真的弹窗并阻塞到窗口被关闭，不适合进 CI。
    /// 跑法：`cargo test -p clipsync-app --bin clipsync -- --ignored injection`
    ///
    /// 判据（三条都要看到，缺一不算通过）：
    ///   1. 窗口真的出现了；
    ///   2. 标题与正文里那串载荷**原样显示**（说明内容确实传进去了——
    ///      光看"命令没报错"会漏掉参数根本没传到这件事）；
    ///   3. `%TEMP%\clipsync_inject_marker.txt` **不存在**（说明没被执行）。
    #[test]
    #[ignore = "会弹窗并阻塞，需人工/脚本关闭"]
    fn manual_injection_dialog() {
        super::show_info(
            &format!("ClipSync 配对失败 {EVIL_NAME}"),
            &format!("对端设备：{EVIL_NAME}\n\n上面这行应原样显示，且不应有任何命令被执行。"),
        );
    }
}
