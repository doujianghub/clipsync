//! 托盘上的配对交互：主持、加入、解除。
//!
//! 与 `pairing_cli` 的分工：那边是协议流程（监听、握手、落盘），这边是
//! **用户交互**——弹哪些窗、什么时候弹、配对成功后还要动哪些运行期状态。
//!
//! 所有流程都跑在后台线程：弹窗会阻塞到用户点掉，放在托盘事件循环里会让
//! 整个菜单卡住。
//!
//! 本文件留主持方与设备组操作；另外两块各自成文件（按行数约定拆分）：
//!   - [`session`]——主持会话的槽位：当前码、剩余时间、取消；
//!   - [`join`]——「输入配对码…」那一侧的交互。

use anyhow::Result;
use clipsync_net::peer::AddrSource;
use tracing::{info, warn};

use crate::{addrbook, config, dialog, hub, known_peers, pairing_cli, tray};

#[path = "pairing_join.rs"]
mod join;
#[path = "pairing_session.rs"]
mod session;

pub(crate) use join::join_by_code_interactive;
pub(crate) use session::PairingHostSlot;

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
    /// 托盘不用管：菜单首行的台数与设备子菜单都挂在设备表的台数与版本号上，
    /// 这里一 upsert 它们自己就跟着变了。
    pub(crate) fn register(&self, record: &clipsync_net::pairing::PairingRecord) {
        self.known.upsert(record.clone().into());
        if !record.addrs.is_empty() {
            self.addrbook.add_addrs(
                &record.device,
                record.addrs.iter().copied(),
                AddrSource::Pairing,
            );
        }
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

/// 托盘「显示配对码…」的完整流程，含单例控制与「换个配对码」。
///
/// 重复点击时**不再** bind 一个已被占用的端口（那会给用户抛 `os error 10048`
/// 且只能重启程序恢复），而是把当前会话的配对码重新弹出来——这本就是用户
/// 重复点击时想要的。
///
/// **循环而不是递归开新线程**：用户点「换个配对码」时旧会话还占着 47685，
/// 新起一路去 bind 是撞车。留在同一个线程里 `continue`，`host()` 已经返回、
/// guard 也已析构，端口必然是空的。
pub(crate) fn host_pairing_interactive(
    dir: &std::path::Path,
    identity: &clipsync_net::crypto::StaticIdentity,
    device_name: &str,
    sync_port: u16,
    pairing: &PairingDeps,
) {
    loop {
        let Some(guard) = pairing.host_slot.try_acquire() else {
            // 已有会话在等待——把**同一份**文案再显示一遍。
            //
            // 此前这里另写了一段更短的，不含地址列表，于是"关掉窗口再打开，
            // 地址就没了"，用户以为程序把信息弄丢了。同一件事只该有一份文案。
            info!("配对会话已在进行中，重新显示当前配对码");
            show_code_dialog(&pairing.host_slot, sync_port);
            return;
        };

        let slot = pairing.host_slot.clone();
        let slot_for_dialog = pairing.host_slot.clone();
        let result = pairing_cli::host(
            dir,
            identity,
            device_name,
            sync_port,
            &guard.cancel,
            |code, deadline| {
                slot.begin(code, deadline);
                // 弹窗会一直阻塞到用户点掉，而配对握手要我们主动 accept 并
                // 收发——占着这个线程的后果是：对方 TCP 连上了（内核替我们
                // 完成三次握手，连接躺在 backlog 里），发来 PAKE 却没人读，
                // 他那边一直等到超时。用户看到的现象就是"必须先点掉配对码
                // 窗口，别人才连得上"。所以弹窗必须与 accept 并行。
                let body = pairing_cli::code_dialog_body(
                    code,
                    sync_port,
                    deadline.saturating_duration_since(std::time::Instant::now()),
                );
                let code = code.to_string();
                std::thread::spawn(move || {
                    if dialog::ask_action("ClipSync 配对", &body, new_code_label()) {
                        request_new_code(&slot_for_dialog, Some(&code));
                    }
                });
            },
        );
        drop(guard); // 显式释放槽位，后续弹窗期间允许再次发起

        match result {
            Ok(record) => {
                pairing.register(&record);
                dialog::show_info(
                    "ClipSync 配对成功",
                    &format!("已与「{}」配对，现在可以互相同步了。", record.name),
                );
                return;
            }
            // 换码：不是故障，接着开下一轮，不弹任何"失败"。
            Err(e) if e.downcast_ref::<pairing_cli::Cancelled>().is_some() => {
                info!("用户要求换一个配对码，正在开始新一轮");
                continue;
            }
            Err(e) => {
                warn!("配对失败: {e:#}");
                dialog::show_info("ClipSync 配对失败", &format!("{e:#}"));
                return;
            }
        }
    }
}

/// 「换个配对码」在菜单与弹窗上的统一叫法。
///
/// 不叫「刷新」：刷新听起来像"重新拿一遍同一个东西"，而这里旧码当场作废。
pub(crate) fn new_code_label() -> &'static str {
    clipsync_core::t!("换个配对码", "New code")
}

/// 显示当前会话的配对码；窗口上的「换个配对码」照常可用。
///
/// 与首次显示走的是同一份文案与同一个按钮——用户不该因为"这是第二次打开"
/// 就少一个选项。
fn show_code_dialog(slot: &PairingHostSlot, sync_port: u16) {
    let Some(live) = slot.live() else {
        dialog::show_info("ClipSync 配对", "配对会话正在启动，请稍候再看。");
        return;
    };
    let body = pairing_cli::code_dialog_body(&live.code, sync_port, live.remaining);
    if dialog::ask_action("ClipSync 配对", &body, new_code_label()) {
        request_new_code(slot, Some(&live.code));
    }
}

/// 请求换码，并在请求落空时说明原因。
///
/// 落空的情形是弹窗停在屏幕上、而它那一轮早已结束（超时或已配对成功）。
/// 静默无反应会让人以为按钮坏了。
fn request_new_code(slot: &PairingHostSlot, only_if: Option<&str>) {
    if slot.request_new_code(only_if) {
        return;
    }
    info!("「{}」落空：对应的配对会话已经结束", new_code_label());
    dialog::show_info(
        "ClipSync 配对",
        "这个配对码已经失效了。\n\n请在托盘菜单里重新选「显示配对码…」。",
    );
}

/// 托盘菜单里的「换个配对码」。
///
/// 与弹窗上那个按钮不同，这里**不限定码**：用户看着菜单上的倒计时点下去，
/// 想换的就是眼前这一轮。
pub(crate) fn new_code_from_tray(pairing: &PairingDeps) {
    request_new_code(&pairing.host_slot, None);
}
