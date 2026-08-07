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
            // 已有会话在等待——把**同一份**文案再显示一遍。
            //
            // 此前这里另写了一段更短的，不含地址列表，于是"关掉窗口再打开，
            // 地址就没了"，用户以为程序把信息弄丢了。同一件事只该有一份文案。
            info!("配对会话已在进行中，重新显示当前配对码");
            dialog::show_info(
                "ClipSync 配对",
                &pairing_cli::code_dialog_body(&code, sync_port),
            );
            return;
        }
    };

    let slot = pairing.host_slot.clone();
    let result = pairing_cli::host(dir, identity, device_name, sync_port, true, |share| {
        slot.set_code(share);
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
