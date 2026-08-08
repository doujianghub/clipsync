//! 本机设备名的获取。
//!
//! 这个名字在配对时发给对端并被其持久化，是用户在 `list` 与托盘里辨认
//! "哪台机器"的唯一依据，因此不能轻易退化成占位符。

/// 尽力获取本机设备名（跨平台，不引额外依赖）。
///
/// 这个名字会在配对时发给对端并被其持久化，是用户在 `list` 与托盘里辨认
/// "哪台机器"的唯一依据，因此不能轻易退化成占位符。
///
/// 依次尝试：
///   1. `CLIPSYNC_DEVICE_NAME` —— 用户显式指定，优先级最高；
///   2. `COMPUTERNAME` —— Windows 由系统设置，可靠；
///   3. `scutil --get ComputerName` —— macOS 上用户在"设置 › 通用 › 关于本机"
///      里看到的那个名字（可含空格与中文），比主机名更贴近用户认知；
///   4. `hostname` 命令 —— 各 Unix 通用兜底，去掉 `.local` 之类的域名后缀；
///   5. `HOSTNAME` 环境变量 —— 某些 shell 会导出。
///
/// **为什么不能只看环境变量**：`HOSTNAME` 是 bash 的 shell 变量，默认并不
/// 导出；zsh 根本不设它，从 launchd/Finder 启动更是没有。实测在 macOS 上
/// 两个变量都不存在，原实现必然退化为 `unknown-host`——两台 Mac 配对后
/// 彼此都显示同一个名字，无法区分。
pub(crate) fn device_name_best_effort() -> String {
    if let Some(name) = non_empty(std::env::var("CLIPSYNC_DEVICE_NAME").ok()) {
        return name;
    }
    if let Some(name) = non_empty(std::env::var("COMPUTERNAME").ok()) {
        return name;
    }

    #[cfg(target_os = "macos")]
    if let Some(name) = non_empty(run_capture("scutil", &["--get", "ComputerName"])) {
        return name;
    }

    #[cfg(unix)]
    if let Some(name) = non_empty(run_capture("hostname", &[])) {
        // `hostname` 常返回 `foo.local` / FQDN，取首段更适合展示。
        let short = name.split('.').next().unwrap_or(&name).to_string();
        if let Some(short) = non_empty(Some(short)) {
            return short;
        }
    }

    if let Some(name) = non_empty(std::env::var("HOSTNAME").ok()) {
        return name;
    }
    "unknown-host".to_string()
}

/// 去掉首尾空白；结果为空则视为"没取到"。
fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 运行一个命令并取其标准输出。命令不存在或失败返回 `None`。
///
/// 只在启动时调用一次，进程开销可忽略；换来的是不必为取一个主机名引入
/// `libc`/`hostname` 依赖。
#[cfg(unix)]
fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_empty_trims_and_rejects_blank() {
        assert_eq!(non_empty(Some("  mac  ".into())), Some("mac".to_string()));
        assert_eq!(non_empty(Some("   ".into())), None);
        assert_eq!(non_empty(Some(String::new())), None);
        assert_eq!(non_empty(None), None);
    }

    /// 回归：本机必须能取到一个真实设备名。
    ///
    /// 原实现只查 `COMPUTERNAME`/`HOSTNAME`，两者在 macOS 上都不存在
    /// （`HOSTNAME` 是 bash 的 shell 变量，不导出；zsh 不设），于是设备名
    /// 恒为 `unknown-host`——多台 Mac 配对后彼此重名，无法分辨。
    ///
    /// 这条断言在 Windows（`COMPUTERNAME`）与 Unix（`scutil`/`hostname`）
    /// 上都应成立。
    #[test]
    fn device_name_is_not_placeholder_on_this_machine() {
        let name = device_name_best_effort();
        assert_ne!(
            name, "unknown-host",
            "未能取到本机设备名，配对后对端将无法分辨这台机器"
        );
        assert!(!name.trim().is_empty(), "设备名不应为空白");
    }

    /// 设备名不应带 `.local` 之类的域名后缀——展示用，越短越清楚。
    #[cfg(unix)]
    #[test]
    fn unix_device_name_has_no_domain_suffix() {
        // 仅在回退到 `hostname` 这条路径时才需要截断；显式指定或 scutil
        // 的结果本就不带后缀，故这里只断言最终结果不含点分域名形态。
        if std::env::var_os("CLIPSYNC_DEVICE_NAME").is_some() {
            return; // 用户显式指定的名字原样保留，不做断言
        }
        let name = device_name_best_effort();
        assert!(
            !name.ends_with(".local"),
            "设备名残留了 .local 后缀: {name}"
        );
    }
}
