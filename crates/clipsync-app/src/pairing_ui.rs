//! 托盘上的配对交互：主持、加入、解除。
//!
//! 与 `pairing_cli` 的分工：那边是协议流程（监听、握手、落盘），这边是
//! **用户交互**——弹哪些窗、什么时候弹、配对成功后还要动哪些运行期状态。
//!
//! 三个流程都跑在后台线程：弹窗会阻塞到用户点掉，放在托盘事件循环里会让
//! 整个菜单卡住。

use std::sync::{Arc, Mutex};

use anyhow::Result;
use clipsync_net::peer::AddrSource;
use tracing::{info, warn};

use crate::{addrbook, config, dialog, hub, known_peers, pairing_cli, tray};

/// 保证同一时刻只有一个"主持配对"会话在跑。
///
/// 没有这层控制时，用户第二次点「显示配对码」会新起一个线程去 bind 已被占用
/// 的 47685，直接抛出 `os error 10048`（Windows）/ `48`（macOS）给用户看。
/// 而用户的真实意图通常只是**再看一眼那个码**——所以这里不报错、也不新开
/// 会话，而是把当前会话的配对码重新弹出来。
#[derive(Clone, Default)]
pub(crate) struct PairingHostSlot {
    /// 会话进行中时持有当前配对码；结束后自动清空。
    pub(crate) active: Arc<Mutex<Option<String>>>,
}

impl PairingHostSlot {
    /// 尝试占用槽位。已被占用时返回当前会话的配对码。
    pub(crate) fn try_acquire(&self) -> Result<PairingHostGuard, String> {
        let mut g = self.active.lock().unwrap();
        match g.as_ref() {
            Some(code) => Err(code.clone()),
            None => {
                *g = Some(String::new()); // 先占位，拿到码后再补
                Ok(PairingHostGuard {
                    slot: self.clone(),
                })
            }
        }
    }

    pub(crate) fn set_code(&self, code: &str) {
        *self.active.lock().unwrap() = Some(code.to_string());
    }

    pub(crate) fn release(&self) {
        *self.active.lock().unwrap() = None;
    }
}

/// 持有期间槽位被占用；**丢弃即释放**，包括 host() 提前返回错误的路径。
pub(crate) struct PairingHostGuard {
    pub(crate) slot: PairingHostSlot,
}

impl Drop for PairingHostGuard {
    fn drop(&mut self) {
        self.slot.release();
    }
}

/// 从托盘发起配对时，"配对成功"之后还需要让它**立刻**生效所需的东西。
#[derive(Clone)]
pub(crate) struct PairingDeps {
    pub(crate) known: known_peers::KnownPeers,
    pub(crate) addrbook: addrbook::AddrBook,
    pub(crate) status: tray::TrayStatus,
    /// 主持会话的单例槽位，避免重复点击撞上端口占用。
    pub(crate) host_slot: PairingHostSlot,
    /// 中枢句柄：移出设备时通知它断开对应连接。
    pub(crate) hub: hub::HubHandle,
    /// 配置目录：配对表的增删要落盘。
    pub(crate) dir: std::path::PathBuf,
    /// 本机的 device id——退出设备组时要告诉对端"删的是我"。
    pub(crate) local_device: clipsync_core::DeviceId,
}

impl PairingDeps {
    /// 登记一台刚配对成功的设备。
    ///
    /// 两处都要更新，缺一台设备就连不上：
    ///   - **设备表**——拨号线程据此决定拨谁，入站握手据此认证对端；
    ///   - **地址簿**——配对时对方告知的可达地址，是首次连接的唯一线索
    ///     （信标只覆盖同网段）。
    ///
    /// 顺带刷新托盘上的已配对台数，否则菜单首行会停在"尚未配对设备"，
    /// 而同步其实已经跑起来了。
    pub(crate) fn register(&self, record: &clipsync_net::pairing::PairingRecord) {
        self.known.upsert(record.clone().into());
        if !record.addrs.is_empty() {
            self.addrbook
                .add_addrs(&record.device, record.addrs.iter().copied(), AddrSource::Pairing);
        }
        self.status.set_paired(self.known.len());
        info!(
            "已登记新配对设备 {} ({})，无需重启即可开始同步",
            record.name, record.device
        );
    }

    /// 当前已配对设备及其在线状态，供托盘渲染设备列表。
    pub(crate) fn peers_for_tray(&self) -> Vec<tray::TrayPeer> {
        let connected = self.status.connected_devices();
        // 来源只存在配对记录里（设备表是运行期的精简副本），读一次盘。
        // 每次打开菜单几十字节的 IO，换用户能分辨"这台是谁引荐来的"。
        let introduced: std::collections::HashMap<String, String> =
            config::load_pairings(&self.dir)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|r| r.introduced_by.map(|by| (r.device.to_string(), by)))
                .collect();

        self.known
            .snapshot()
            .into_iter()
            .map(|p| {
                let id = p.device.to_string();
                tray::TrayPeer {
                    online: connected.contains(id.as_str()),
                    introduced_by: introduced.get(&id).cloned(),
                    device: id,
                    name: p.name,
                }
            })
            .collect()
    }

    /// 把某台设备移出设备组。
    ///
    /// 引荐让若干设备构成了一个组，所以移出也是**组语义**：本机清干净之后，
    /// 还要告诉所有在线成员一起清——只清自己没用，下一轮别人就把它引荐回来了。
    ///
    /// 本机这边四处都要清，漏一处就会留下"移出了但还在连"或"列表里没了却仍被
    /// 信标接纳"这类半吊子状态：
    ///   - **磁盘记录**：否则重启后它又回来了；
    ///   - **设备表**：拨号线程与入站认证都查它，清掉才算真的断绝关系；
    ///   - **地址簿**：留着会让诊断输出显示一台已移出的设备；
    ///   - **中枢**：丢掉发送通道，当场切断已建立的连接。
    pub(crate) fn remove_peer(&self, device_id: &str) -> Result<Option<String>> {
        let device = clipsync_core::DeviceId::from_hex(device_id);
        let name = config::remove_pairing(&self.dir, &device)?;
        if name.is_none() {
            return Ok(None); // 已经不在了，无需再做
        }
        // 先广播后断开：切断之后中枢就没有通往它的通道了，那台设备自己
        // 收不到通知。
        self.hub.send(hub::HubEvent::AnnounceRemoval {
            device: device.clone(),
        });
        self.forget_locally(&device);
        self.status.set_paired(self.known.len());
        Ok(name)
    }

    /// 本机退出设备组：清空全部配对，并告知其它成员把本机删掉。
    ///
    /// 返回退出前的设备台数。不通知对端的话，它们会带着一条永远连不上的
    /// 记录一直重试，列表里也永远挂着一台离线设备。
    pub(crate) fn leave_group(&self) -> Result<usize> {
        let peers = self.known.snapshot();
        self.hub.send(hub::HubEvent::AnnounceRemoval {
            device: self.local_device.clone(),
        });
        for p in &peers {
            self.forget_locally(&p.device);
        }
        config::clear_pairings(&self.dir)?;
        self.status.set_paired(0);
        Ok(peers.len())
    }

    /// 从本机的三处运行期状态里抹掉一台设备（磁盘记录由调用方负责）。
    fn forget_locally(&self, device: &clipsync_core::DeviceId) {
        self.known.remove(device);
        self.addrbook.forget(device);
        self.hub.send(hub::HubEvent::Unpaired {
            device: device.clone(),
        });
    }
}


/// 托盘里点某台设备 → 确认 → 把它移出设备组。
///
/// 先确认再动手：这会影响组里所有设备，不只是本机。
pub(crate) fn remove_peer_interactive(pairing: &PairingDeps, device_id: &str) {
    // 名字从当前设备表取，弹窗里要让用户看清移出的是哪一台。
    let name = pairing
        .known
        .snapshot()
        .into_iter()
        .find(|p| p.device.as_str() == device_id)
        .map(|p| p.name)
        .unwrap_or_else(|| device_id.to_string());

    // 说清这是**全组**操作。只写"本机不再同步"会让人以为别人那边还留着，
    // 而实际上其它设备也会一起把它删掉。
    if !dialog::confirm(
        "移出设备组",
        &format!(
            "确定把「{name}」移出设备组吗？\n\n\
             组里所有设备都会移除它，它自己也会清空配对。\n\
             想加回来的话，重新配对一次即可。"
        ),
    ) {
        return;
    }

    match pairing.remove_peer(device_id) {
        Ok(Some(name)) => {
            info!("已把 {name} 移出设备组");
            dialog::show_info("ClipSync", &format!("已把「{name}」移出设备组。"));
        }
        Ok(None) => info!("设备 {device_id} 已不在配对列表中，无需移出"),
        Err(e) => {
            warn!("移出设备失败: {e:#}");
            dialog::show_info("ClipSync", &format!("移出失败：{e}"));
        }
    }
}

/// 托盘里点「退出设备组」→ 确认 → 本机离开。
pub(crate) fn leave_group_interactive(pairing: &PairingDeps) {
    let count = pairing.known.len();
    if count == 0 {
        dialog::show_info("ClipSync", "本机尚未配对任何设备。");
        return;
    }

    if !dialog::confirm(
        "退出设备组",
        &format!(
            "确定退出设备组吗？\n\n\
             本机将清空全部 {count} 台配对，其它设备也会移除本机。\n\
             想回来的话，重新配对一次即可。"
        ),
    ) {
        return;
    }

    match pairing.leave_group() {
        Ok(n) => {
            info!("已退出设备组，清空 {n} 台配对");
            dialog::show_info("ClipSync", "已退出设备组。");
        }
        Err(e) => {
            warn!("退出设备组失败: {e:#}");
            dialog::show_info("ClipSync", &format!("退出失败：{e}"));
        }
    }
}

/// 托盘「显示配对码…」的完整流程，含单例控制。
///
/// 重复点击时**不再** bind 一个已被占用的端口（那会给用户抛 `os error 10048`
/// 且只能重启程序恢复），而是把当前会话的配对码重新弹出来——这本就是用户
/// 重复点击时想要的。
pub(crate) fn host_pairing_interactive(
    dir: &std::path::Path,
    identity: &clipsync_net::crypto::StaticIdentity,
    device_name: &str,
    sync_port: u16,
    pairing: &PairingDeps,
) {
    let guard = match pairing.host_slot.try_acquire() {
        Ok(g) => g,
        Err(code) => {
            // 已有会话在等待——把同一个码再显示一遍即可。
            info!("配对会话已在进行中，重新显示当前配对码");
            dialog::show_info_and_copy(
                "ClipSync 配对",
                &format!(
                    "配对码：{code}\n（已复制到剪贴板）\n\n\
                     本机仍在等待对方加入，请在对方设备上选「输入配对码…」。"
                ),
                &code,
            );
            return;
        }
    };

    let slot = pairing.host_slot.clone();
    let result = pairing_cli::host(dir, identity, device_name, sync_port, true, |code| {
        slot.set_code(code.as_str());
    });
    drop(guard); // 显式释放槽位，后续弹窗期间允许再次发起

    match result {
        Ok(record) => {
            pairing.register(&record);
            dialog::show_info(
                "ClipSync 配对成功",
                &format!("已与「{}」配对，现在可以互相同步了。", record.name),
            );
        }
        Err(e) => {
            warn!("配对失败: {e:#}");
            dialog::show_info("ClipSync 配对失败", &format!("{e:#}"));
        }
    }
}

/// 托盘「输入配对码…」的完整流程：拿到码 → 加入 → 告知结果。
///
/// 跑在后台线程（弹窗会阻塞到用户点掉，不能占着托盘事件循环）。
///
/// **优先读剪贴板**：对方点「显示配对码」时，码就已经自动进了他的剪贴板；
/// 他微信发给你、你复制一下——这是本来就要做的动作。于是这里直接读，
/// 弹个确认框问一句就行，不必手打 6 位码（也就不会打错）。
///
/// 这还顺带绕开了一个真实故障：Windows 上文本输入框依赖 PowerShell 子进程，
/// 而那条路径可能被杀软拦下（`powershell -Command -` 是无文件攻击的典型
/// 模式）。确认框走的是系统原生 TaskDialog，不受影响——**配对这条核心流程
/// 因此不再依赖输入框**。
///
/// **地址从哪来**，按代价从低到高：
///   1. 码串里就带着（`ABCDEF@100.88.88.22`）——跨网络时的正路；
///   2. 局域网自动发现——同网段最省事；
///   3. 都不行才问用户要地址。
pub(crate) fn join_by_code_interactive(
    dir: &std::path::Path,
    identity: &clipsync_net::crypto::StaticIdentity,
    device_name: &str,
    sync_port: u16,
    pairing: &PairingDeps,
) {
    let Some((code, host)) = obtain_code() else {
        return; // 用户取消，或没给出可用的码
    };
    let code = code.to_string();

    // 码串里带了地址就直连，不必再试注定失败的组播发现。
    if let Some(host) = host {
        finish_join(
            pairing_cli::join(dir, identity, device_name, Some(&host), &code, sync_port),
            pairing,
        );
        return;
    }

    match pairing_cli::join(dir, identity, device_name, None, &code, sync_port) {
        Ok(record) => {
            finish_join(Ok(record), pairing);
            return;
        }
        Err(e) => info!("局域网未发现对方，改为询问地址: {e:#}"),
    }

    let Some(host) = dialog::prompt(
        "输入配对码",
        "没能在局域网里找到对方。\n请输入对方的 IP（对方窗口里有）：",
    ) else {
        return;
    };
    finish_join(
        pairing_cli::join(dir, identity, device_name, Some(&host), &code, sync_port),
        pairing,
    );
}

/// 拿到配对码：先看剪贴板，不成再让用户手输。
fn obtain_code() -> Option<(clipsync_net::pairing::PairingCode, Option<String>)> {
    if let Some(text) = clipboard_text() {
        if let Some((code, host)) = pairing_cli::parse_pairing_input(&text) {
            let where_ = match &host {
                Some(h) => format!("\n对方地址：{h}"),
                None => String::new(),
            };
            if dialog::confirm(
                "输入配对码",
                &format!("剪贴板里的配对码是 {code}{where_}\n\n用它与对方配对吗？"),
            ) {
                return Some((code, host));
            }
            // 用户说不是这个，继续往下走手输。
        }
    }

    let input = dialog::prompt(
        "输入配对码",
        "填入对方显示的配对码。\n跨网络时用对方给的「码@地址」那一串。",
    )?;
    match pairing_cli::parse_pairing_input(&input) {
        Some(v) => Some(v),
        None => {
            dialog::show_info(
                "配对失败",
                &format!("「{input}」不是有效的配对码。\n\n应为 6 位码，或「码@地址」。"),
            );
            None
        }
    }
}

/// 读剪贴板里的纯文本；读不到就当没有。
fn clipboard_text() -> Option<String> {
    use clipsync_clip::Clipboard as _;
    let mut cb = clipsync_clip::ArboardClipboard::new().ok()?;
    match cb.read().ok()?? .content {
        clipsync_core::ClipContent::Text(s) => Some(s),
        _ => None,
    }
}

/// 配对收尾：登记 + 告知结果。三条路径共用。
fn finish_join(
    result: Result<clipsync_net::pairing::PairingRecord>,
    pairing: &PairingDeps,
) {
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


#[cfg(test)]
mod tests {
    use super::*;

    /// 重复发起主持配对时，不得去 bind 一个已被占用的端口。
    ///
    /// 用户第二次点「显示配对码」通常只是想再看一眼码。原先每次点击都新起
    /// 线程去 bind 47685，第二次必然失败并把 `os error 10048` 抛给用户看，
    /// 且只能重启程序恢复。槽位被占用时应返回**当前会话的码**供重新显示。
    #[test]
    fn host_slot_reports_existing_code_instead_of_starting_over() {
        let slot = PairingHostSlot::default();

        let guard = slot.try_acquire().expect("首次应能占用");
        slot.set_code("ABC123");

        match slot.try_acquire() {
            Err(code) => assert_eq!(code, "ABC123", "应返回当前会话的码以便重新显示"),
            Ok(_) => panic!("已有会话时不应再次占用槽位——那会撞上端口占用"),
        }

        // 会话结束后必须能重新发起，否则功能就永久坏掉了。
        drop(guard);
        assert!(
            slot.try_acquire().is_ok(),
            "会话结束后槽位应释放，允许发起新一轮配对"
        );
    }

    /// 槽位在 `host()` 报错返回时也要释放（靠 guard 的 Drop，不能靠成功路径）。
    #[test]
    fn host_slot_releases_on_error_path() {
        let slot = PairingHostSlot::default();
        {
            let _guard = slot.try_acquire().expect("应能占用");
            slot.set_code("XYZ789");
            // 模拟 host() 中途 bail!——guard 在作用域结束时析构。
        }
        assert!(
            slot.try_acquire().is_ok(),
            "出错路径也必须释放槽位，否则一次配对失败就再也发起不了"
        );
    }
}
