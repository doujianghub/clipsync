//! 启动期的接线：局域网发现、手动地址、剪贴板监听线程。
//!
//! 这些都是"把各部件连起来"的一次性动作，与 `main` 的命令分发无关，
//! 单独放一处便于查阅启动时到底起了哪些线程。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use clipsync_clip::{ArboardClipboard, Clipboard, ClipboardWatcher, PollingWatcher};
use clipsync_net::peer::AddrSource;
use tracing::{info, warn};

use crate::{addrbook, hub, known_peers};

/// 启动局域网信标：宣告本机 + 发现同网段的已配对设备。
pub(crate) fn start_discovery(
    device_id: &clipsync_core::DeviceId,
    addrbook: &addrbook::AddrBook,
    known: &known_peers::KnownPeers,
    sync_port: u16,
) {
    use clipsync_net::discovery;

    // 发送：周期性宣告本机身份与全部可达地址。
    let sender_port = sync_port;
    if let Err(e) = discovery::spawn_sender(device_id.clone(), sync_port, move || {
        clipsync_net::local::local_candidates(sender_port)
    }) {
        warn!("启动信标发送失败（局域网发现不可用）: {e:#}");
    }

    // 接收：只接纳已配对设备的信标，其余忽略。
    //
    // 这里持有的是共享的设备表而非启动时的快照：运行期新配对的设备，其信标
    // 也应当立刻被接纳，否则新设备要等到重启才会被局域网发现。
    let known = known.clone();
    let book = addrbook.clone();
    match discovery::spawn_listener(device_id.clone(), move |found| {
        if !known.contains(&found.device) {
            return; // 非已配对设备，忽略
        }
        // 信标源 IP 是最可靠的局域网地址；同时收下对端宣告的其它地址。
        book.add_addrs(&found.device, [found.lan_addr], AddrSource::Beacon);
        book.add_addrs(&found.device, found.extra_addrs, AddrSource::Beacon);
    }) {
        Ok(_) => {}
        Err(e) => warn!(
            "启动信标监听失败（同机多实例时属正常，将依赖配对/通告地址）: {e:#}"
        ),
    }
}

/// 把 `CLIPSYNC_PEERS` 指定的地址作为手动来源加入地址簿。
///
/// 用于自动发现无法覆盖的场景（如公网端口转发、固定 DDNS）。格式 `ip:port`，
/// 逗号分隔；会为每个已配对设备都加入这些候选（握手时的公钥认证会拒绝错配）。
pub(crate) fn seed_manual_addrs(
    addrbook: &addrbook::AddrBook,
    pairings: &[clipsync_net::pairing::PairingRecord],
) {
    let raw = match std::env::var("CLIPSYNC_PEERS") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return,
    };
    let addrs: Vec<std::net::SocketAddr> = raw
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    if addrs.is_empty() {
        return;
    }
    for p in pairings {
        addrbook.add_addrs(&p.device, addrs.iter().copied(), AddrSource::Manual);
    }
    info!("已加入 {} 个手动指定地址（CLIPSYNC_PEERS）", addrs.len());
}

/// 启动剪贴板监听线程：本地变化 → 读取 → 投递给中枢。
///
/// `received_dir` 是本程序落地接收文件的目录。若一次剪贴板变化的文件**全部**
/// 位于其中，说明这是我们自己刚写入剪贴板的接收结果，不应再广播回去——否则
/// 两台设备会来回推送同一批文件形成回环。
///
/// 文本与图片的回环由引擎的哈希登记（`expect_echo`）拦截；文件走这条路径是
/// 因为落地后的文件修改时间与源文件不同，标识必然改变，无法用哈希比对识别。
pub(crate) fn spawn_clip_watch(
    hub: hub::HubHandle,
    running: Arc<AtomicBool>,
    received_dir: std::path::PathBuf,
) -> Result<()> {
    let mut reader = ArboardClipboard::new()?;
    let mut watcher = PollingWatcher::new()?;
    std::thread::Builder::new()
        .name("clip-watch".into())
        .spawn(move || {
            let r = watcher.run(&mut || {
                if !running.load(Ordering::SeqCst) {
                    return false;
                }
                // 大图从系统剪贴板取出来要现场解码（macOS 上是 TIFF），
                // 一张 4K 截图能占掉可观的时间——慢的时候得看得见。
                let t0 = std::time::Instant::now();
                let got = reader.read();
                crate::logging::note_slow("读取剪贴板", t0);
                match got {
                    Ok(Some(read)) => {
                        if is_our_received_files(&read, &received_dir) {
                            tracing::debug!("跳过本机刚落地的接收文件（避免回环）");
                            return true;
                        }
                        warn_once_about_denied(&read.denied);
                        hub.send(hub::HubEvent::Local {
                            content: read.content,
                            sensitive: read.sensitive,
                            file_paths: read.file_paths,
                        });
                    }
                    Ok(None) => {}
                    Err(e) => warn!("读取剪贴板失败: {e:#}"),
                }
                true
            });
            if let Err(e) = r {
                warn!("剪贴板监听线程退出: {e:#}");
            }
        })?;
    Ok(())
}

/// 把本次复制里被拒的文件报给用户（只报第一个，一次说清即可）。
fn warn_once_about_denied(denied: &[clipsync_clip::DeniedFile]) {
    if let Some(d) = denied.first() {
        crate::dialog::permission_hint_once(&d.path, &d.reason, &d.where_to_fix);
    }
}

/// 这次剪贴板内容是否为本程序自己落地的接收文件。
pub(crate) fn is_our_received_files(read: &clipsync_clip::ClipRead, received_dir: &std::path::Path) -> bool {
    !read.file_paths.is_empty()
        && read
            .file_paths
            .iter()
            .all(|p| p.starts_with(received_dir))
}

#[cfg(test)]
#[path = "wiring_tests.rs"]
mod tests;
