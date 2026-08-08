//! 「输入配对码…」这一侧的交互。
//!
//! 与 `pairing_ui` 主文件的分工：那边是主持方（出码、等人连），这边是发起方
//! （拿码、找人、连过去）。两侧的失败处理很不一样——主持方的失败是"没人来"，
//! 发起方的失败要区分"找不到路"和"码不对"，各自的话术都不短。

use anyhow::Result;
use tracing::{info, warn};

use super::PairingDeps;
use crate::{dialog, pairing_cli};

/// 托盘「输入配对码…」的完整流程：拿到码 → 找到对方 → 配对 → 告知结果。
///
/// 跑在后台线程（弹窗会阻塞到用户点掉，不能占着托盘事件循环）。
///
/// **码只能人念人敲**，所以是 4 位数字。曾经走过一条弯路：把码自动放进
/// 主持方的剪贴板，让用户"复制粘贴过去"——这是循环依赖，本工具要解决的
/// 正是"跨设备复制粘贴还没打通"；也不该假设用户手边有微信之类的通道。
///
/// **地址一律自动找**，用户不必输 IP（见 [`pairing_cli::connect_hosts`]）。
/// 只有全部落空才问一句，那时对方窗口里也正好印着本机地址。
pub(crate) fn join_by_code_interactive(
    dir: &std::path::Path,
    identity: &clipsync_net::crypto::StaticIdentity,
    device_name: &str,
    sync_port: u16,
    pairing: &PairingDeps,
) {
    // 循环：找不到对方时最可能是码过期了，那就让用户拿新码再来一遍——
    // 而不是把他推去填 IP。对方根本没在监听的话，IP 填对了也连不上。
    loop {
        let Some((code, host)) = obtain_code() else {
            return; // 用户取消，或输入的不是有效配对码
        };
        let code = code.to_string();

        let streams = match pairing_cli::connect_hosts(host.as_deref()) {
            Ok(s) => s,
            Err(e) => {
                info!("没找到等待配对的设备: {e:#}");
                match ask_what_next() {
                    Some(NotFound::RetryWithNewCode) => continue,
                    Some(NotFound::EnterAddress) => {
                        let Some(h) = dialog::prompt("配对", &ask_address_body(sync_port)) else {
                            return;
                        };
                        match pairing_cli::connect_hosts(Some(h.trim())) {
                            Ok(s) => s,
                            Err(e) => {
                                warn!("配对失败: {e:#}");
                                dialog::show_info("配对失败", &format!("{e:#}"));
                                return;
                            }
                        }
                    }
                    None => return,
                }
            }
        };

        // 逐个试：占着配对端口的不一定就是 ClipSync，通常只有一个。
        let mut last = None;
        for mut s in streams {
            match pairing_cli::join_on(&mut s, dir, identity, device_name, &code, sync_port) {
                Ok(record) => return finish_join(Ok(record), pairing),
                Err(e) => last = Some(e),
            }
        }
        return finish_join(
            Err(last.unwrap_or_else(|| anyhow::anyhow!("没有可用的连接"))),
            pairing,
        );
    }
}

/// 没找到对方时，用户接下来想干什么。
enum NotFound {
    /// 让对方重新显示配对码，自己再输一次新的。
    RetryWithNewCode,
    /// 手动填对方地址。
    EnterAddress,
}

/// 手输对方地址时的提示语。
///
/// 一并列出**本机**的地址：对方窗口里可能有好几个，用户得挑一个跟自己同
/// 网段的才连得通。不给参照物的话，这个判断只能靠猜——而这恰恰是程序能
/// 帮上忙、用户又最容易搞错的地方。
fn ask_address_body(sync_port: u16) -> String {
    let mut s = "请输入对方的 IP（对方窗口里有）：".to_string();
    if let Some(block) = pairing_cli::addr_block(sync_port) {
        s.push_str(&format!("\n\n本机地址，供对照挑同网段的：\n{block}"));
    }
    s
}

/// 没找到对方时问一句下一步。
///
/// **把最可能的原因说在前面**：自动发现覆盖到局域网、覆盖网与可枚举的虚拟
/// 网段，都落空的话，绝大多数时候不是"找不到路"，而是"对方那边已经过期了"
/// ——配对码只有 3 分钟有效。此前这里直接弹出"请输入对方 IP"，把用户引向一条
/// 死路：对方根本没在监听，IP 填得再对也连不上。
fn ask_what_next() -> Option<NotFound> {
    let opts = [
        "让对方重新显示配对码，我输新的".to_string(),
        "手动填对方地址".to_string(),
    ];
    match dialog::choose(
        "没找到对方",
        &format!(
            "没找到正在等待配对的设备。\n\n\
             配对码 {} 分钟内有效，多半是过期了。",
            pairing_cli::HOST_SESSION_TIMEOUT.as_secs() / 60
        ),
        &opts,
    ) {
        Some(0) => Some(NotFound::RetryWithNewCode),
        Some(1) => Some(NotFound::EnterAddress),
        _ => None,
    }
}

/// 让用户输入配对码。
///
/// 也接受 `1234@地址` 这种写法——自动发现全落空时的手动出口，命令行同款。
fn obtain_code() -> Option<(clipsync_net::pairing::PairingCode, Option<String>)> {
    let input = dialog::prompt("输入配对码", "输入对方显示的 4 位配对码：")?;
    match pairing_cli::parse_pairing_input(&input) {
        Some(v) => Some(v),
        None => {
            dialog::show_info(
                "配对失败",
                &format!("「{input}」不是有效的配对码。\n\n应为 4 位数字。"),
            );
            None
        }
    }
}

/// 配对收尾：登记 + 告知结果。各路径共用。
fn finish_join(result: Result<clipsync_net::pairing::PairingRecord>, pairing: &PairingDeps) {
    match result {
        Ok(record) => {
            pairing.register(&record);
            dialog::show_info("配对成功", &format!("已与「{}」配对。", record.name));
        }
        Err(e) => {
            warn!("配对失败: {e:#}");
            dialog::show_info("配对失败", &format!("{e:#}"));
        }
    }
}
