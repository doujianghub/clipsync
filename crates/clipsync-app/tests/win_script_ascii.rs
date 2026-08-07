//! PowerShell 脚本必须是纯 ASCII。
//!
//! 我们把脚本从 stdin 喂给 `powershell -Command -`，而 **Windows PowerShell
//! 5.1 默认按系统 ANSI 编码读 stdin**（中文系统是 GBK）。脚本里只要有一个
//! 中文字符，UTF-8 字节就会被按 GBK 解释成乱码——引号一旦被破坏就是语法
//! 错误，脚本直接退出。
//!
//! 症状极具迷惑性：PowerShell 进程确实启动了（能看到黑框一闪），但窗口
//! 永远不出现，日志里也没有任何错误——因为出错的是子进程，而它的 stderr
//! 被我们丢弃了。实际排查时正是靠"黑框一闪而过"这条线索才定位到。
//!
//! 所有面向用户的中文（标题、正文、按钮文字）一律经**环境变量**传入：
//! 那条通道是 Unicode 的，不受 stdin 编码影响。
//!
//! 放在集成测试里是因为 `dialog_win.rs` 只在 Windows 上编译，而这条约束
//! 在任何平台上都该被检查——尤其是在 macOS 上开发、改不到也测不到它的时候。

#[test]
fn powershell_scripts_contain_no_non_ascii() {
    let src = include_str!("../src/dialog_win.rs");

    let mut checked = 0;
    let mut offenders = Vec::new();
    // 扫所有 r#"..."# 原始字符串常量——脚本都是这么写的。
    for chunk in src.split("r#\"").skip(1) {
        let Some(body) = chunk.split("\"#").next() else {
            continue;
        };
        checked += 1;
        let bad: String = body.chars().filter(|c| !c.is_ascii()).collect();
        if !bad.is_empty() {
            offenders.push(bad);
        }
    }

    assert!(checked > 0, "没扫到任何脚本常量，测试本身失效了");
    assert!(
        offenders.is_empty(),
        "PowerShell 脚本里出现了非 ASCII 字符：{offenders:?}\n\
         中文必须经环境变量传入，写在脚本体里会被 GBK 误解码导致脚本失败。"
    );
}
