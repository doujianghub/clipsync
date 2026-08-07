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

/// 弹框的同时把 `copy_text` 放入剪贴板，省去用户手抄。
///
/// 注意：这会改变系统剪贴板，进而触发本程序自身的监听。调用方需自行
/// 决定是否可接受（配对码是短文本，且配对时通常尚无对端可同步）。
pub fn show_info_and_copy(title: &str, body: &str, copy_text: &str) {
    use clipsync_clip::Clipboard as _;
    match clipsync_clip::ArboardClipboard::new() {
        Ok(mut cb) => {
            let content = clipsync_core::ClipContent::Text(copy_text.to_string());
            if let Err(e) = cb.write(&content) {
                tracing::debug!("配对码写入剪贴板失败（不影响弹窗）: {e:#}");
            }
        }
        Err(e) => tracing::debug!("打开剪贴板失败（不影响弹窗）: {e:#}"),
    }
    show_info(title, body);
}

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

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::{Context, Result};
    use std::process::Command;

    /// 脚本体不含任何用户内容：标题与正文经 `argv` 传入，天然免疫注入。
    const SCRIPT: &str = r#"on run argv
  display dialog (item 2 of argv) with title (item 1 of argv) buttons {"好"} default button 1 with icon note
end run"#;

    pub fn show(title: &str, body: &str) -> Result<()> {
        run_osascript(SCRIPT, title, body).map(|_| ())
    }

    /// 带输入框的对话框。
    ///
    /// `text returned` 经 stdout 返回给我们。用户点「取消」时 osascript 以
    /// 错误 -128 退出，我们据此返回 `None`——这不是故障，无需报错。
    const PROMPT_SCRIPT: &str = r#"on run argv
  set r to display dialog (item 2 of argv) with title (item 1 of argv) default answer "" buttons {"取消", "确定"} default button 2 with icon note
  return text returned of r
end run"#;

    pub fn prompt(title: &str, body: &str) -> Result<Option<String>> {
        run_osascript(PROMPT_SCRIPT, title, body)
    }

    /// 确认框。默认按钮刻意设为「取消」——用它的都是破坏性操作
    /// （解除配对），手快连按回车不该把事情做了。
    const CONFIRM_SCRIPT: &str = r#"on run argv
  display dialog (item 2 of argv) with title (item 1 of argv) buttons {"取消", "确定"} default button 1 with icon caution
  return button returned of result
end run"#;

    pub fn confirm(title: &str, body: &str) -> Result<bool> {
        // 用户点「取消」时 osascript 以 -128 退出 → None → false。
        Ok(run_osascript(CONFIRM_SCRIPT, title, body)?
            .map(|s| s.trim() == "确定")
            .unwrap_or(false))
    }

    /// 运行一段 osascript，脚本经 stdin 送入、用户内容经 argv 传参。
    ///
    /// 返回 `Ok(None)` 表示脚本以非零码退出——对话框场景下就是用户取消。
    fn run_osascript(script: &str, title: &str, body: &str) -> Result<Option<String>> {
        use std::io::Write;

        let mut child = Command::new("osascript")
            .arg("-")
            .arg(title)
            .arg(body)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("启动 osascript 失败")?;

        child
            .stdin
            .take()
            .context("取 osascript stdin 失败")?
            .write_all(script.as_bytes())
            .context("写入 osascript 脚本失败")?;

        let out = child
            .wait_with_output()
            .context("等待 osascript 结束失败")?;
        if !out.status.success() {
            return Ok(None); // 用户取消
        }
        Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
    }

    #[cfg(test)]
    mod tests {
        /// 参数必须真的送达脚本，返回值必须真的取回来。
        ///
        /// 这条不弹窗——用一段只做回显的脚本，把整条传参与取值链路走一遍。
        ///
        /// **为什么单独测这个**：项目里已经栽过一次同类问题——Windows 侧
        /// 当时验证了"命令没报错"就判定通过，而参数根本没传进去，弹窗内容
        /// 是空的。"没有观察到失败"不等于"验证通过"，得有一个**应该看到的
        /// 正向信号**：这里就是回显出来的那两个值。
        #[test]
        fn arguments_reach_the_script_and_output_comes_back() {
            const ECHO: &str = r#"on run argv
  return (item 1 of argv) & "|" & (item 2 of argv)
end run"#;
            let out = super::run_osascript(ECHO, "标题A", "正文B").expect("osascript 应可运行");
            let out = out.expect("回显脚本应正常退出并有输出");
            assert!(
                out.contains("标题A") && out.contains("正文B"),
                "参数未送达脚本，实际输出: {out:?}"
            );
        }

        /// 脚本以非零码退出时返回 `None`——对话框场景下即用户点了取消。
        #[test]
        fn nonzero_exit_maps_to_cancelled() {
            const FAIL: &str = r#"on run argv
  error "cancelled" number -128
end run"#;
            let out = super::run_osascript(FAIL, "t", "b").expect("调用本身不应报错");
            assert!(out.is_none(), "用户取消应表现为 None，而不是空字符串或错误");
        }

        /// 注入防护：恶意内容必须原样回显，且不产生任何执行痕迹。
        ///
        /// 载荷取自 `dialog::tests::EVIL_NAME` 的同类手法——设备名来自对端，
        /// 属不可信输入。这条能自动跑，不必依赖人工看窗口。
        #[test]
        fn malicious_content_is_data_not_code() {
            const ECHO: &str = r#"on run argv
  return item 2 of argv
end run"#;
            let marker = std::env::temp_dir().join("clipsync_osa_inject_marker.txt");
            let _ = std::fs::remove_file(&marker);

            let payload = format!(
                r#"设备"X" & (do shell script "touch {}") & "结束""#,
                marker.display()
            );
            let out = super::run_osascript(ECHO, "标题", &payload)
                .expect("应可运行")
                .expect("应有输出");

            assert!(
                out.contains("do shell script"),
                "载荷应原样回显（说明是数据），实际: {out:?}"
            );
            assert!(
                !marker.exists(),
                "载荷被当作脚本执行了——注入防护失效"
            );
        }
    }
}

#[cfg(windows)]
mod platform {
    use anyhow::{Context, Result};
    use std::process::Command;

    /// **必须在创建任何窗口之前执行**的 DPI 感知声明。
    ///
    /// PowerShell 进程默认**不是** DPI 感知的。在缩放比例非 100% 的屏幕上
    /// （笔记本常见 125%/150%），Windows 会把非感知进程的窗口按低分辨率绘制
    /// 再**位图拉伸**上去——窗口里的文字因此发虚、有像素感。而标题栏由系统
    /// DWM 独立绘制，始终清晰，于是呈现出"标题很锐利、内容很糊"的割裂感。
    ///
    /// 优先声明 PerMonitorV2（`-4`，Win10 1703+）：多显示器且缩放不同时，
    /// 窗口跨屏拖动也能重新按目标屏的 DPI 渲染。旧系统上该调用失败，退回
    /// `SetProcessDPIAware`（系统级 DPI 感知），效果对单屏用户等价。
    ///
    /// `-ErrorAction SilentlyContinue`：这只是清晰度优化，任何一步失败都不
    /// 该让弹窗本身弹不出来——糊一点也远好过看不见配对码。
    const DPI_PRELUDE: &str = r#"try {
  Add-Type -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
'@ -Name Dpi -Namespace ClipSyncNative -ErrorAction SilentlyContinue | Out-Null
  if (-not [ClipSyncNative.Dpi]::SetProcessDpiAwarenessContext([IntPtr]::new(-4))) {
    [ClipSyncNative.Dpi]::SetProcessDPIAware() | Out-Null
  }
} catch { }
"#;

    /// 内容经**环境变量**传入，脚本体不做任何字符串拼接。
    ///
    /// 不用位置参数：`-Command -` 从 stdin 读脚本，**不会**把尾随参数传给
    /// `param()`——实测确认该写法会让弹窗内容为空（配对码看不见，正是本次
    /// 要修的症状）。环境变量既避开这一点，也同样不存在注入面：值永远不会
    /// 被当作脚本文本解析。
    const SCRIPT: &str = r#"Add-Type -AssemblyName System.Windows.Forms | Out-Null
[System.Windows.Forms.Application]::EnableVisualStyles()
[System.Windows.Forms.MessageBox]::Show($env:CLIPSYNC_DLG_BODY, $env:CLIPSYNC_DLG_TITLE, 'OK', 'Information') | Out-Null
"#;

    /// 输入框：WinForms 手搭，因为 `MessageBox` 没有输入框。
    ///
    /// 几处刻意为之：
    ///   - **字体显式设为 Segoe UI 9pt**。WinForms 的默认字体是
    ///     `Microsoft Sans Serif`——那是 Win9x 时代的位图字体，在现代系统上
    ///     与其它窗口格格不入且发虚。Segoe UI 是 Windows 自 Vista 起的系统
    ///     界面字体，与资源管理器、设置面板一致。
    ///   - **`AutoScaleMode = Dpi`**，配合 [`DPI_PRELUDE`] 让控件按真实 DPI
    ///     布局，而不是先按 96dpi 排好再整体拉伸。
    ///   - **输入框用等宽字体**：配对码含数字与字母，等宽下 `0/O`、`1/l`
    ///     更容易分辨，用户照着念给对方时不易出错。
    ///   - `AcceptButton`/`CancelButton`：回车确定、Esc 取消，符合直觉。
    ///
    /// 结果经 stdout 返回；取消时不输出任何内容。
    const PROMPT_SCRIPT: &str = r#"Add-Type -AssemblyName System.Windows.Forms, System.Drawing | Out-Null
[System.Windows.Forms.Application]::EnableVisualStyles()

$form = New-Object System.Windows.Forms.Form
$form.Text = $env:CLIPSYNC_DLG_TITLE
$form.Font = New-Object System.Drawing.Font('Segoe UI', 9)
$form.AutoScaleMode = [System.Windows.Forms.AutoScaleMode]::Dpi
$form.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::FixedDialog
$form.StartPosition = [System.Windows.Forms.FormStartPosition]::CenterScreen
$form.MaximizeBox = $false
$form.MinimizeBox = $false
$form.ClientSize = New-Object System.Drawing.Size(380, 150)

$label = New-Object System.Windows.Forms.Label
$label.Text = $env:CLIPSYNC_DLG_BODY
$label.SetBounds(16, 16, 348, 54)
$label.AutoSize = $false
$form.Controls.Add($label)

$box = New-Object System.Windows.Forms.TextBox
$box.SetBounds(16, 76, 348, 28)
$box.Font = New-Object System.Drawing.Font('Consolas', 12)
$form.Controls.Add($box)

$ok = New-Object System.Windows.Forms.Button
$ok.Text = '确定'
$ok.DialogResult = [System.Windows.Forms.DialogResult]::OK
$ok.SetBounds(196, 114, 80, 26)
$form.Controls.Add($ok)

$cancel = New-Object System.Windows.Forms.Button
$cancel.Text = '取消'
$cancel.DialogResult = [System.Windows.Forms.DialogResult]::Cancel
$cancel.SetBounds(284, 114, 80, 26)
$form.Controls.Add($cancel)

$form.AcceptButton = $ok
$form.CancelButton = $cancel
$form.Topmost = $true
$form.Add_Shown({ $box.Focus() | Out-Null })

if ($form.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
  [Console]::Out.Write($box.Text)
}
"#;

    pub fn show(title: &str, body: &str) -> Result<()> {
        run_powershell(SCRIPT, title, body, false).map(|_| ())
    }

    pub fn prompt(title: &str, body: &str) -> Result<Option<String>> {
        run_powershell(PROMPT_SCRIPT, title, body, true)
    }

    /// 确认框。默认按钮设为「否」——破坏性操作不该被一次回车带过。
    const CONFIRM_SCRIPT: &str = r#"Add-Type -AssemblyName System.Windows.Forms | Out-Null
[System.Windows.Forms.Application]::EnableVisualStyles()
$r = [System.Windows.Forms.MessageBox]::Show(
  $env:CLIPSYNC_DLG_BODY, $env:CLIPSYNC_DLG_TITLE,
  [System.Windows.Forms.MessageBoxButtons]::YesNo,
  [System.Windows.Forms.MessageBoxIcon]::Warning,
  [System.Windows.Forms.MessageBoxDefaultButton]::Button2)
if ($r -eq [System.Windows.Forms.DialogResult]::Yes) { [Console]::Out.Write("yes") }
"#;

    pub fn confirm(title: &str, body: &str) -> Result<bool> {
        Ok(run_powershell(CONFIRM_SCRIPT, title, body, true)?
            .map(|s| s.trim() == "yes")
            .unwrap_or(false))
    }

    /// 运行一段 PowerShell 脚本，用户内容经环境变量传入。
    ///
    /// `capture` 为真时收集 stdout 作为返回值；脚本非零退出或无输出时返回
    /// `Ok(None)`（对话框场景即用户取消）。
    fn run_powershell(
        script: &str,
        title: &str,
        body: &str,
        capture: bool,
    ) -> Result<Option<String>> {
        use std::io::Write;

        let stdout = if capture {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        };
        let mut child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", "-"])
            .env("CLIPSYNC_DLG_TITLE", title)
            .env("CLIPSYNC_DLG_BODY", body)
            .stdin(std::process::Stdio::piped())
            .stdout(stdout)
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("启动 powershell 失败")?;

        // DPI 声明必须先于脚本体：一旦窗口已创建，再改感知级别就晚了。
        let mut stdin = child.stdin.take().context("取 powershell stdin 失败")?;
        stdin
            .write_all(DPI_PRELUDE.as_bytes())
            .and_then(|()| stdin.write_all(script.as_bytes()))
            .context("写入 powershell 脚本失败")?;
        drop(stdin); // 关闭 stdin，否则 `-Command -` 会一直等更多输入

        if !capture {
            child.wait().context("等待 powershell 结束失败")?;
            return Ok(None);
        }
        let out = child
            .wait_with_output()
            .context("等待 powershell 结束失败")?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use anyhow::Result;

    pub fn show(title: &str, body: &str) -> Result<()> {
        // 无统一的弹窗方案，退化为日志。
        tracing::info!("[{title}] {body}");
        Ok(())
    }

    pub fn prompt(title: &str, body: &str) -> Result<Option<String>> {
        // 无图形环境可用，调用方会退回命令行路径。
        tracing::info!("[{title}] {body}（本平台无输入框，请用命令行 `clipsync pair <配对码>`）");
        Ok(None)
    }

    pub fn confirm(title: &str, body: &str) -> Result<bool> {
        // 无从征得同意，一律当作否——破坏性操作不能默认执行。
        tracing::info!("[{title}] {body}（本平台无确认框，按取消处理）");
        Ok(false)
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
    #[test]
    #[ignore = "会弹窗并阻塞，需人工/脚本关闭"]
    fn manual_prompt_dialog() {
        let r = super::prompt(
            "ClipSync 配对",
            "请输入对方显示的配对码：\n（在对方设备的托盘菜单里选「显示配对码…」）",
        );
        println!("RESULT={r:?}");
    }

    #[test]
    #[ignore = "会弹窗并阻塞，需人工/脚本关闭"]
    fn manual_injection_dialog() {
        super::show_info(
            &format!("ClipSync 配对失败 {EVIL_NAME}"),
            &format!("对端设备：{EVIL_NAME}\n\n上面这行应原样显示，且不应有任何命令被执行。"),
        );
    }
}
