//! 托盘与业务逻辑之间的桥接：把菜单动作接到各处实现上。
//!
//! `tray` 模块只管画菜单、报告"用户点了什么"；真正要做的事在这里组装。
//! 分开是因为前者是平台 UI 代码，后者纯粹是本程序的业务编排。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use tracing::{info, warn};

use crate::pairing_ui::PairingDeps;
use crate::{autostart, config, dialog, logging, pairing_ui, size_parse, tray};

/// 在主线程运行托盘，处理菜单动作直到用户退出。
pub(crate) fn run_tray(
    status: tray::TrayStatus,
    running: Arc<AtomicBool>,
    dir: std::path::PathBuf,
    identity: clipsync_net::crypto::StaticIdentity,
    device_name: String,
    sync_port: u16,
    settings: config::SettingsHandle,
    pairing: PairingDeps,
    log_control: logging::LogControl,
    hub_thread: std::thread::JoinHandle<()>,
) -> Result<()> {
    let pairing_for_peers = pairing.clone();
    let status_for_cb = status.clone();
    let running_for_cb = running.clone();
    let settings_for_cb = settings.clone();
    let settings_for_read = settings.clone();

    let callbacks = tray::TrayCallbacks {
        on_action: Box::new(move |action| match action {
            tray::TrayAction::TogglePause => {
                let now = !status_for_cb.is_paused();
                status_for_cb.set_paused(now);
                info!("同步已{}", if now { "暂停" } else { "恢复" });
                true
            }
            tray::TrayAction::ToggleAutostart => {
                let now = !autostart::is_enabled();
                match autostart::set_enabled(now) {
                    Ok(()) => info!("开机自启已{}", if now { "开启" } else { "关闭" }),
                    Err(e) => warn!("设置开机自启失败: {e:#}"),
                }
                true
            }
            tray::TrayAction::ShowPairingCode => {
                // 在后台线程主持配对，避免阻塞托盘事件循环。
                let dir = dir.clone();
                let identity = identity.clone();
                let name = device_name.clone();
                let pairing = pairing.clone();
                std::thread::spawn(move || {
                    pairing_ui::host_pairing_interactive(&dir, &identity, &name, sync_port, &pairing);
                });
                true
            }
            tray::TrayAction::EnterPairingCode => {
                let dir = dir.clone();
                let identity = identity.clone();
                let name = device_name.clone();
                let pairing = pairing.clone();
                std::thread::spawn(move || {
                    pairing_ui::join_by_code_interactive(&dir, &identity, &name, sync_port, &pairing);
                });
                true
            }
            tray::TrayAction::Quit => {
                info!("正在退出…");
                running_for_cb.store(false, Ordering::SeqCst);
                false
            }
            tray::TrayAction::ToggleImages => {
                let now = !settings_for_cb.snapshot().allow_image;
                settings_for_cb.update(|s| s.allow_image = now);
                info!("同步图片已{}", if now { "开启" } else { "关闭" });
                true
            }
            tray::TrayAction::ToggleFiles => {
                let now = !settings_for_cb.snapshot().allow_files;
                settings_for_cb.update(|s| s.allow_files = now);
                info!("同步文件已{}", if now { "开启" } else { "关闭" });
                true
            }
            tray::TrayAction::ToggleCompress => {
                let now = !settings_for_cb.snapshot().compress_transfers;
                settings_for_cb.update(|s| s.compress_transfers = now);
                info!("传输前压缩已{}", if now { "开启" } else { "关闭" });
                true
            }
            // 弹框会阻塞到用户点掉，放在托盘事件循环里会让整个菜单卡住，
            // 故一律丢到后台线程。
            tray::TrayAction::PromptMaxBytes => {
                let s = settings_for_cb.clone();
                std::thread::spawn(move || prompt_max_bytes(&s));
                true
            }
            tray::TrayAction::PromptUploadLimit => {
                let s = settings_for_cb.clone();
                std::thread::spawn(move || prompt_upload_limit(&s));
                true
            }
            tray::TrayAction::PromptListenPort => {
                let s = settings_for_cb.clone();
                std::thread::spawn(move || prompt_listen_port(&s));
                true
            }
            tray::TrayAction::ToggleVerboseLog => {
                let now = !settings_for_cb.snapshot().verbose_log;
                match log_control.set_verbose(now) {
                    Ok(()) => settings_for_cb.update(|s| s.verbose_log = now),
                    // 切不动就别改设置，免得界面显示"已开启"而实际没生效。
                    Err(e) => warn!("切换日志级别失败: {e:#}"),
                }
                true
            }
            tray::TrayAction::OpenLogDir => {
                if let Err(e) = logging::open_in_file_manager(log_control.dir()) {
                    warn!("打开日志文件夹失败: {e:#}");
                    crate::dialog::show_info(
                        "ClipSync 日志",
                        &format!("日志位于：\n{}\n\n（自动打开失败：{e}）", log_control.dir().display()),
                    );
                }
                true
            }
            tray::TrayAction::Unpair(device_id) => {
                // 确认框会阻塞到用户点掉，丢到后台线程免得卡住托盘。
                let pairing = pairing.clone();
                std::thread::spawn(move || pairing_ui::unpair_interactive(&pairing, &device_id));
                true
            }
        }),
        current_settings: Box::new(move || {
            let s = settings_for_read.snapshot();
            tray::TraySettings {
                sync_images: s.allow_image,
                sync_files: s.allow_files,
                compress: s.compress_transfers,
                max_bytes: s.max_bytes,
                upload_limit: s.upload_limit_bytes_per_sec,
                listen_port: s.listen_port,
                verbose_log: s.verbose_log,
            }
        }),
        current_peers: Box::new(move || pairing_for_peers.peers_for_tray()),
        // 中枢线程一旦结束（正常退出或 panic），同步就全停了。托盘据此
        // 切到故障状态，而不是继续显示一切正常。
        hub_alive: Box::new(move || !hub_thread.is_finished()),
    };

    if let Err(e) = tray::run(status, callbacks) {
        // 托盘不可用（无桌面会话等）时退化为纯后台运行，不影响同步。
        warn!("托盘不可用，转为纯后台运行: {e:#}");
        while running.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    Ok(())
}

/// 托盘里点某台设备 → 确认 → 解除配对。
///

/// 弹一个输入框改某项设置。
///
/// 三处设置（上限、限速、端口）流程完全一样：显示当前值 → 收输入 → 解析 →
/// 写回或告知为什么没生效。差别只在**怎么解析**和**怎么描述**，抽出来后
/// 各自只剩几行。
///
/// 输入非法时**再弹一次说明**而不是静默忽略：用户刚打完一串字，什么反馈
/// 都没有只会让人以为程序坏了。
fn prompt_setting<T>(
    settings: &config::SettingsHandle,
    title: &str,
    current: &str,
    hint: &str,
    parse: impl Fn(&str) -> anyhow::Result<T>,
    apply: impl FnOnce(&mut config::Settings, T),
) {
    let Some(input) = dialog::prompt(title, &format!("当前：{current}\n\n{hint}")) else {
        return; // 用户取消
    };
    match parse(&input) {
        Ok(v) => {
            settings.update(|s| apply(s, v));
            info!("{title}已更新");
        }
        Err(e) => dialog::show_info(title, &format!("{e}")),
    }
}

fn prompt_max_bytes(settings: &config::SettingsHandle) {
    let cur = settings.snapshot().max_bytes;
    let shown = if cur == usize::MAX {
        "不限".to_string()
    } else {
        tray::human_bytes(cur as u64)
    };
    prompt_setting(
        settings,
        "单次上限",
        &shown,
        "例如 500MB、1.5GiB；填 0 表示不限",
        size_parse::parse_byte_size,
        // 0 在这里表示"不限"，而不是"一个字节都不许传"。
        |s, v| s.max_bytes = if v == 0 { usize::MAX } else { v as usize },
    );
}

fn prompt_upload_limit(settings: &config::SettingsHandle) {
    let cur = settings.snapshot().upload_limit_bytes_per_sec;
    let shown = if cur == 0 {
        "不限".to_string()
    } else {
        format!("{}/s", tray::human_bytes(cur))
    };
    prompt_setting(
        settings,
        "发送限速",
        &shown,
        "例如 10MB/s、20mbps；填 0 表示不限",
        size_parse::parse_rate,
        |s, v| s.upload_limit_bytes_per_sec = v,
    );
}

/// 端口与其它设置不同：**改了要重启才生效**。监听套接字在启动时就绑好了，
/// 运行中换端口意味着断开所有连接重新监听，还要让对端重新学到新端口。
/// 与其做一套半可靠的热切换，不如如实告诉用户重启一下。
fn prompt_listen_port(settings: &config::SettingsHandle) {
    let cur = settings.snapshot().listen_port;
    prompt_setting(
        settings,
        "同步端口",
        &cur.to_string(),
        "填 1024–65535；重启后生效",
        |s| {
            let p: u16 = s
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("「{s}」不是有效端口"))?;
            // 1024 以下是特权端口，普通用户绑不上，提前拦住比启动时才失败好。
            if p < 1024 {
                anyhow::bail!("端口 {p} 属于系统保留范围，请填 1024–65535");
            }
            Ok(p)
        },
        |s, v| s.listen_port = v,
    );
}

