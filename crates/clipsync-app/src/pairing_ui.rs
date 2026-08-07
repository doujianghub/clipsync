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
    /// 中枢句柄：解除配对时通知它断开对应连接。
    pub(crate) hub: hub::HubHandle,
    /// 配置目录：解除配对要落盘。
    pub(crate) dir: std::path::PathBuf,
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
        self.known
            .snapshot()
            .into_iter()
            .map(|p| tray::TrayPeer {
                online: connected.contains(p.device.as_str()),
                device: p.device.to_string(),
                name: p.name,
            })
            .collect()
    }

    /// 解除与某台设备的配对。
    ///
    /// 四处都要清，漏一处就会留下"解除了但还在连"或"列表里没了却仍被信标
    /// 接纳"这类半吊子状态：
    ///   - **磁盘记录**：否则重启后它又回来了；
    ///   - **设备表**：拨号线程与入站认证都查它，清掉才算真的断绝关系；
    ///   - **地址簿**：留着会让诊断输出显示一台已解除的设备；
    ///   - **中枢**：丢掉发送通道，当场切断已建立的连接。
    pub(crate) fn unpair(&self, device_id: &str) -> Result<Option<String>> {
        let device = clipsync_core::DeviceId::from_hex(device_id);
        let name = config::remove_pairing(&self.dir, &device)?;
        if name.is_none() {
            return Ok(None); // 已经不在了，无需再做
        }
        self.known.remove(&device);
        self.addrbook.forget(&device);
        self.hub.send(hub::HubEvent::Unpaired {
            device: device.clone(),
        });
        self.status.set_paired(self.known.len());
        Ok(name)
    }
}


/// 托盘里点某台设备 → 确认 → 解除配对。
///
/// 先确认再动手：这是不可撤销的操作，解除后要重新走一遍配对流程才能恢复。
pub(crate) fn unpair_interactive(pairing: &PairingDeps, device_id: &str) {
    // 名字从当前设备表取，弹窗里要让用户看清解除的是哪一台。
    let name = pairing
        .known
        .snapshot()
        .into_iter()
        .find(|p| p.device.as_str() == device_id)
        .map(|p| p.name)
        .unwrap_or_else(|| device_id.to_string());

    if !dialog::confirm(
        "ClipSync 解除配对",
        &format!(
            "确定要解除与「{name}」的配对吗？\n\n\
             解除后双方将立即断开、不再同步。\n\
             要恢复需要重新走一次配对流程。"
        ),
    ) {
        return;
    }

    match pairing.unpair(device_id) {
        Ok(Some(name)) => {
            info!("已解除与 {name} 的配对");
            dialog::show_info("ClipSync", &format!("已解除与「{name}」的配对。"));
        }
        Ok(None) => info!("设备 {device_id} 已不在配对列表中，无需解除"),
        Err(e) => {
            warn!("解除配对失败: {e:#}");
            dialog::show_info("ClipSync 解除配对失败", &format!("{e:#}"));
        }
    }
}

/// 弹框自定义单次大小上限。
///
/// 输入非法时**再弹一次说明**而不是静默忽略：用户刚打完一串字，什么反馈都

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

/// 托盘「输入配对码…」的完整流程：要码 → 加入 → 告知结果。
///
/// 跑在后台线程（弹窗会阻塞到用户点掉，不能占着托盘事件循环）。
///
/// **地址从哪来**：先按局域网自动发现找主持方；找不到再问一次对方地址——
/// 主持方那边的窗口里就列着可用地址，照抄即可。这样同局域网的常见情形
/// 全程只需输一个配对码，跨网络也不至于卡死没有出路。
pub(crate) fn join_by_code_interactive(
    dir: &std::path::Path,
    identity: &clipsync_net::crypto::StaticIdentity,
    device_name: &str,
    sync_port: u16,
    pairing: &PairingDeps,
) {
    let Some(code) = dialog::prompt(
        "ClipSync 配对",
        "请输入对方显示的配对码：\n（在对方设备的托盘菜单里选「显示配对码…」）",
    ) else {
        return; // 用户取消
    };

    if clipsync_net::pairing::PairingCode::parse(&code).is_none() {
        dialog::show_info(
            "ClipSync 配对失败",
            &format!("配对码「{code}」格式不正确，请核对后重试。"),
        );
        return;
    }

    // 先试局域网自动发现。
    match pairing_cli::join(dir, identity, device_name, None, &code, sync_port) {
        Ok(record) => {
            pairing.register(&record);
            dialog::show_info(
                "ClipSync 配对成功",
                &format!("已与「{}」配对，现在可以互相同步了。", record.name),
            );
            return;
        }
        Err(e) => {
            info!("局域网自动发现未能完成配对，改为询问对方地址: {e:#}");
        }
    }

    // 自动发现走不通（不同局域网、组播被拦），退而求其次问地址。
    let Some(host) = dialog::prompt(
        "ClipSync 配对",
        "没能在局域网里找到对方。\n请输入对方设备的 IP 地址（对方窗口里有列出）：",
    ) else {
        return;
    };

    match pairing_cli::join(dir, identity, device_name, Some(&host), &code, sync_port) {
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
