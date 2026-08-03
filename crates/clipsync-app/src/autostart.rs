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
    platform::set_enabled(enable, &exe)
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
            exe = exe.display()
        );
        std::fs::write(&path, plist).context("写入 LaunchAgent 失败")?;
        Ok(())
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
