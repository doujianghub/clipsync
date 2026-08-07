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

/// Base64 编码必须与标准一致。
///
/// 它是自己写的二十行，而错了的后果极其隐蔽：PowerShell 解不开
/// `-EncodedCommand` 只会静默退出，既不报错也不弹窗——正是我们排查了好几轮
/// 的那个症状。所以这里拿标准向量对一遍。
///
/// 实现在 `dialog_win.rs`（只在 Windows 编译），这里照抄一份对拍——它足够短，
/// 抄一份比为了测试把平台代码搬出来划算。
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
            out.push(if chunk.len() > 1 { TABLE[(n >> 6 & 63) as usize] as char } else { '=' });
            out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
        }
        out
    }

    // RFC 4648 的标准测试向量，覆盖三种补位情况。
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foo"), "Zm9v");
    assert_eq!(base64(b"foob"), "Zm9vYg==");
    assert_eq!(base64(b"fooba"), "Zm9vYmE=");
    assert_eq!(base64(b"foobar"), "Zm9vYmFy");

    // PowerShell 要的是 UTF-16LE 再 Base64。`Get-Date` 的标准编码结果——
    // 这是微软文档里的例子，能对上说明整条链路（编码 + 补位）都对。
    let utf16: Vec<u8> = "Get-Date".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    assert_eq!(base64(&utf16), "RwBlAHQALQBEAGEAdABlAA==");
}
