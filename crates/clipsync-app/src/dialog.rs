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

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::{Context, Result};
    use std::process::Command;

    /// 脚本体不含任何用户内容：标题与正文经 `argv` 传入，天然免疫注入。
    const SCRIPT: &str = r#"on run argv
  display dialog (item 2 of argv) with title (item 1 of argv) buttons {"好"} default button 1 with icon note
end run"#;

    pub fn show(title: &str, body: &str) -> Result<()> {
        Command::new("osascript")
            .arg("-")
            .arg(title)
            .arg(body)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("启动 osascript 失败")
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .take()
                    .context("取 osascript stdin 失败")?
                    .write_all(SCRIPT.as_bytes())
                    .context("写入 osascript 脚本失败")?;
                child.wait().context("等待 osascript 结束失败")?;
                Ok(())
            })
    }
}

#[cfg(windows)]
mod platform {
    use anyhow::{Context, Result};
    use std::process::Command;

    /// 内容经**环境变量**传入，脚本体不做任何字符串拼接。
    ///
    /// 不用位置参数：`-Command -` 从 stdin 读脚本，**不会**把尾随参数传给
    /// `param()`——实测确认该写法会让弹窗内容为空（配对码看不见，正是本次
    /// 要修的症状）。环境变量既避开这一点，也同样不存在注入面：值永远不会
    /// 被当作脚本文本解析。
    const SCRIPT: &str = r#"Add-Type -AssemblyName System.Windows.Forms | Out-Null
[System.Windows.Forms.MessageBox]::Show($env:CLIPSYNC_DLG_BODY, $env:CLIPSYNC_DLG_TITLE, 'OK', 'Information') | Out-Null
"#;

    pub fn show(title: &str, body: &str) -> Result<()> {
        let mut child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", "-"])
            .env("CLIPSYNC_DLG_TITLE", title)
            .env("CLIPSYNC_DLG_BODY", body)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("启动 powershell 失败")?;

        use std::io::Write;
        child
            .stdin
            .take()
            .context("取 powershell stdin 失败")?
            .write_all(SCRIPT.as_bytes())
            .context("写入 powershell 脚本失败")?;
        child.wait().context("等待 powershell 结束失败")?;
        Ok(())
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
    #[test]
    #[ignore = "会弹窗并阻塞，需人工/脚本关闭"]
    fn manual_injection_dialog() {
        super::show_info(
            &format!("ClipSync 配对失败 {EVIL_NAME}"),
            &format!("对端设备：{EVIL_NAME}\n\n上面这行应原样显示，且不应有任何命令被执行。"),
        );
    }
}
