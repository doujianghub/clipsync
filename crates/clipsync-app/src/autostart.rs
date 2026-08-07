//! 开机自启的开关与状态查询。
//!
//!   - **Windows**：在注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//!     下写入一项。这是用户级自启，无需管理员权限，也不会影响其它用户。
//!   - **macOS**：在 `~/Library/LaunchAgents/` 下写入一个 LaunchAgent plist。
//!
//! 两者都只在当前用户范围内生效，卸载时删除对应项即可，不留系统级残留。

use anyhow::{Context, Result};

/// 注册表/plist 中使用的标识。
const APP_KEY: &str = "ClipSync";

/// 当前是否已设置开机自启。
pub fn is_enabled() -> bool {
    platform::is_enabled().unwrap_or(false)
}

/// 开启或关闭开机自启。
pub fn set_enabled(enable: bool) -> Result<()> {
    let exe = std::env::current_exe().context("获取当前程序路径失败")?;
    if enable {
        warn_if_transient(&exe);
    }
    platform::set_enabled(enable, &exe)
}

/// 自启记录的是当前可执行文件的绝对路径。若它位于构建产物目录，
/// `cargo clean` 或移动仓库都会让自启指向一个不存在的文件而静默失效——
/// 这种失败要到下次开机才会被发现，所以在设置时就提示。
fn warn_if_transient(exe: &std::path::Path) {
    let in_build_dir = exe
        .components()
        .any(|c| c.as_os_str() == "target" || c.as_os_str() == "deps");
    if in_build_dir {
        tracing::warn!(
            "自启将指向构建产物 {}，cargo clean 或移动目录后会失效；\
             正式使用请指向安装后的位置或 .app 内的可执行文件",
            exe.display()
        );
    }
}

#[cfg(windows)]
mod platform {
    use std::path::Path;

    use anyhow::{Context, Result};

    use super::APP_KEY;

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

    pub fn is_enabled() -> Result<bool> {
        // 用 reg query 读取，避免为一个小功能引入注册表操作依赖。
        let out = std::process::Command::new("reg")
            .args(["query", RUN_KEY, "/v", APP_KEY])
            .output()
            .context("查询注册表失败")?;
        Ok(out.status.success())
    }

    pub fn set_enabled(enable: bool, exe: &Path) -> Result<()> {
        // 用 output() 而非 status()：捕获 reg 命令自身的提示输出，避免它
        // 混进本程序的界面/日志。
        let out = if enable {
            // 路径含空格时需要引号，故整体再包一层。
            let value = format!("\"{}\"", exe.display());
            std::process::Command::new("reg")
                .args([
                    "add", RUN_KEY, "/v", APP_KEY, "/t", "REG_SZ", "/d", &value, "/f",
                ])
                .output()
        } else {
            std::process::Command::new("reg")
                .args(["delete", RUN_KEY, "/v", APP_KEY, "/f"])
                .output()
        }
        .context("执行注册表命令失败")?;

        // 关闭时若本就没有该项，reg 会返回失败——这不算错误。
        if enable && !out.status.success() {
            anyhow::bail!(
                "设置开机自启失败: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::path::Path;

    use anyhow::{Context, Result};

    use super::APP_KEY;

    fn plist_path() -> Result<std::path::PathBuf> {
        let home = std::env::var("HOME").context("读取 HOME 失败")?;
        Ok(std::path::PathBuf::from(home)
            .join("Library/LaunchAgents")
            .join(format!("com.{}.plist", APP_KEY.to_lowercase())))
    }

    pub fn is_enabled() -> Result<bool> {
        Ok(plist_path()?.exists())
    }

    pub fn set_enabled(enable: bool, exe: &Path) -> Result<()> {
        let path = plist_path()?;
        if !enable {
            if path.exists() {
                std::fs::remove_file(&path).context("删除 LaunchAgent 失败")?;
            }
            return Ok(());
        }

        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).context("创建 LaunchAgents 目录失败")?;
        }
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
</dict>
</plist>
"#,
            label = APP_KEY.to_lowercase(),
            exe = xml_escape(&exe.display().to_string())
        );
        std::fs::write(&path, plist).context("写入 LaunchAgent 失败")?;
        Ok(())
    }

    /// 转义 XML 文本节点中的特殊字符。
    ///
    /// plist 是 XML，而可执行文件路径来自用户的目录结构——`~/Projects/A&B/`
    /// 这样的目录并不罕见。未转义就拼进去会生成**语法非法的 plist**：
    /// `std::fs::write` 照样成功，`is_enabled()` 也照样返回 true（它只看文件
    /// 是否存在），只有到下次开机时 launchd 才会静默拒绝加载。属于"写的时候
    /// 一切正常、要到重启才发现"的故障，故在写入前就杜绝。
    fn xml_escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&apos;"),
                _ => out.push(c),
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::xml_escape;

        #[test]
        fn escapes_xml_metacharacters() {
            assert_eq!(
                xml_escape("/Users/me/A&B/clip<sync>"),
                "/Users/me/A&amp;B/clip&lt;sync&gt;"
            );
            assert_eq!(xml_escape(r#"/a/"b"/'c'"#), "/a/&quot;b&quot;/&apos;c&apos;");
        }

        #[test]
        fn leaves_ordinary_paths_untouched() {
            let p = "/Users/wang/PythonProject/ClipSync/target/release/clipsync";
            assert_eq!(xml_escape(p), p);
            // 中文路径同样不受影响。
            assert_eq!(xml_escape("/Users/王鑫/应用/clipsync"), "/Users/王鑫/应用/clipsync");
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use std::path::Path;

    use anyhow::Result;

    pub fn is_enabled() -> Result<bool> {
        Ok(false)
    }

    pub fn set_enabled(_enable: bool, _exe: &Path) -> Result<()> {
        anyhow::bail!("本平台暂不支持开机自启设置")
    }
}
