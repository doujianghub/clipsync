//! Windows 弹窗不得再依赖子进程。
//!
//! 早先这里守的是"PowerShell 脚本必须纯 ASCII"——因为脚本从 stdin 喂给
//! `powershell -Command -`，而 Windows PowerShell 5.1 默认按系统 ANSI 编码
//! （中文系统 GBK）读 stdin，混进一个中文就会乱码、语法错、静默退出。
//!
//! 现在整条 PowerShell 路径已被删除（TaskDialog + 自绘 Win32 窗口取代），
//! 那条约束不复存在。改为守住更根本的一条：**弹窗不许再起子进程**。
//!
//! 子进程弹窗的代价在这个项目上反复付过：黑窗一闪、高 DPI 下布局错位、
//! 每次多花几百毫秒、以及最要命的——错误发生在子进程里，主程序日志中
//! 什么都看不到，光定位就耗了三轮。

/// Windows 弹窗模块不得出现 `Command::new`。
#[test]
fn windows_dialogs_spawn_no_subprocess() {
    for (name, src) in [
        ("dialog_win.rs", include_str!("../src/dialog_win.rs")),
        (
            "dialog_win_task.rs",
            include_str!("../src/dialog_win_task.rs"),
        ),
        (
            "dialog_win_input.rs",
            include_str!("../src/dialog_win_input.rs"),
        ),
    ] {
        assert!(
            !src.contains("Command::new"),
            "{name} 里出现了 Command::new——弹窗又退回子进程了。\n\
             那会带来黑窗、DPI 布局错位，且错误只发生在子进程里、主日志看不见。"
        );
    }
}

/// Base64 编码必须与标准一致。
///
/// 虽然 `-EncodedCommand` 那条路已经删了，这段编码逻辑仍留在 TaskDialog
/// 之外的地方吗？——没有了。保留这条测试是因为它几乎零成本，而一旦将来
/// 又需要编码（比如传参给某个外部工具），有现成的对拍可用。
#[test]
fn base64_matches_reference_vectors() {
    fn base64(data: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[(n >> 18 & 63) as usize] as char);
            out.push(TABLE[(n >> 12 & 63) as usize] as char);
            out.push(if chunk.len() > 1 {
                TABLE[(n >> 6 & 63) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        out
    }
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foobar"), "Zm9vYmFy");
}
