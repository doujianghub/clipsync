//! Windows 的弹窗实现（PowerShell + WinForms）。
//!
//! 用户内容一律经**环境变量**传入，脚本体是固定字面量。窗口创建前先声明
//! DPI 感知，否则高缩放屏上文字会因位图拉伸而发虚。

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
