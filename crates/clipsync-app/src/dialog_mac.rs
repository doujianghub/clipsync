//! macOS 的弹窗实现（`osascript`）。
//!
//! 用户内容一律经 **argv** 传入，脚本体是固定字面量——实测确认字符串拼接
//! 可被恶意设备名注入并执行任意命令。

use anyhow::{Context, Result};
use std::process::Command;

/// 脚本体不含任何用户内容：标题与正文经 `argv` 传入，天然免疫注入。
const SCRIPT: &str = r#"on run argv
  display dialog (item 2 of argv) with title (item 1 of argv) buttons {"好"} default button 1 with icon note
end run"#;

pub fn show(title: &str, body: &str) -> Result<()> {
    run_osascript(SCRIPT, title, body).map(|_| ())
}

/// 信息框 + 一个动作按钮。默认按钮是「好」——动作是岔路，不是主路。
///
/// 用户按右上角关闭或 Esc 时 osascript 以 -128 退出 → `None` → 按"没动作"处理。
const ACTION_SCRIPT: &str = r#"on run argv
  display dialog (item 2 of argv) with title (item 1 of argv) buttons {(item 3 of argv), "好"} default button 2 with icon note
  return button returned of result
end run"#;

pub fn ask_action(title: &str, body: &str, action: &str) -> Result<bool> {
    Ok(run_osascript_args(ACTION_SCRIPT, &[title, body, action])?
        .map(|s| s.trim() == action)
        .unwrap_or(false))
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

/// 列表选择。
///
/// 用 `choose from list` 而非 `display dialog`：后者会顶着一个大图标——
/// 而在子进程里跑，那个图标是 **osascript 自己的脚本图标**，看着像在问你
/// 要一个文件路径。`choose from list` 就是一个干净的列表，没有这个问题。
///
/// 返回选中项的**文本**，由调用方换算下标；取消时 AppleScript 返回 false，
/// 这里映射为空串。
const CHOOSE_SCRIPT: &str = r#"on run argv
  set opts to items 3 thru -1 of argv
  set r to choose from list opts with title (item 1 of argv) with prompt (item 2 of argv) default items {item 3 of argv} OK button name "确定" cancel button name "取消"
  if r is false then return ""
  return item 1 of r
end run"#;

pub fn choose(title: &str, body: &str, items: &[String]) -> Result<Option<usize>> {
    let mut args: Vec<&str> = vec![title, body];
    args.extend(items.iter().map(|s| s.as_str()));
    let picked = run_osascript_args(CHOOSE_SCRIPT, &args)?;
    let Some(picked) = picked else {
        return Ok(None);
    };
    let picked = picked.trim();
    if picked.is_empty() {
        return Ok(None); // 用户取消
    }
    Ok(items.iter().position(|s| s == picked))
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
    run_osascript_args(script, &[title, body])
}

/// 运行一段 osascript，脚本经 stdin 送入、**全部**用户内容经 argv 传参。
///
/// 参数化传递是注入防护的关键：脚本体是固定字面量，用户内容永远是数据。
fn run_osascript_args(script: &str, args: &[&str]) -> Result<Option<String>> {
    use std::io::Write;

    let mut child = Command::new("osascript")
        .arg("-")
        .args(args)
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
        assert!(!marker.exists(), "载荷被当作脚本执行了——注入防护失效");
    }
}
