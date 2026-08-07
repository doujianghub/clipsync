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
            tray::TrayAction::ToggleSendImages => {
                let now = !settings_for_cb.snapshot().allow_image;
                settings_for_cb.update(|s| s.allow_image = now);
                info!("发送图片已{}", if now { "开启" } else { "关闭" });
                true
            }
            tray::TrayAction::ToggleSendFiles => {
                let now = !settings_for_cb.snapshot().allow_files;
                settings_for_cb.update(|s| s.allow_files = now);
                info!("发送文件已{}", if now { "开启" } else { "关闭" });
                true
            }
            tray::TrayAction::ToggleCompress => {
                let now = !settings_for_cb.snapshot().compress_transfers;
                settings_for_cb.update(|s| s.compress_transfers = now);
                info!("传输前自动压缩已{}", if now { "开启" } else { "关闭" });
                true
            }
            tray::TrayAction::SetMaxBytes(v) => {
                settings_for_cb.update(|s| s.max_bytes = v);
                if v == usize::MAX {
                    info!("单次大小上限：不限制");
                } else {
                    info!("单次大小上限：{} MiB", v / (1024 * 1024));
                }
                true
            }
            tray::TrayAction::SetUploadLimit(v) => {
                settings_for_cb.update(|s| s.upload_limit_bytes_per_sec = v);
                if v == 0 {
                    info!("发送限速：不限速");
                } else {
                    info!("发送限速：{} MB/s", v / 1_000_000);
                }
                true
            }
            // 三个「自定义…」都要弹框，而弹框会阻塞到用户点掉——放在托盘
            // 事件循环里会让整个菜单卡住，故一律丢到后台线程。
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
                send_images: s.allow_image,
                send_files: s.allow_files,
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

/// 弹框自定义单次大小上限。
///
/// 输入非法时**再弹一次说明**而不是静默忽略：用户刚打完一串字，什么反馈都
/// 没有只会让人以为程序坏了。
fn prompt_max_bytes(settings: &config::SettingsHandle) {
    let current = settings.snapshot().max_bytes;
    let Some(input) = dialog::prompt(
        "ClipSync 单次大小上限",
        &format!(
            "当前：{}\n\n输入新的上限，例如 500MB、1.5GiB、200m：\n（填 0 表示不限制）",
            describe_bytes(current)
        ),
    ) else {
        return;
    };

    match size_parse::parse_byte_size(&input) {
        Ok(0) => {
            settings.update(|s| s.max_bytes = usize::MAX);
            info!("单次大小上限：不限制");
        }
        Ok(v) => {
            let v = v as usize;
            settings.update(|s| s.max_bytes = v);
            info!("单次大小上限：{} 字节", v);
        }
        Err(e) => dialog::show_info("ClipSync 设置未生效", &format!("{e}")),
    }
}

/// 弹框自定义发送限速。
fn prompt_upload_limit(settings: &config::SettingsHandle) {
    let current = settings.snapshot().upload_limit_bytes_per_sec;
    let shown = if current == 0 {
        "不限速".to_string()
    } else {
        format!("{}/s", describe_bytes(current as usize))
    };
    let Some(input) = dialog::prompt(
        "ClipSync 发送限速",
        &format!(
            "当前：{shown}\n\n输入新的限速，例如 10MB/s、20mbps、5M：\n（填 0 表示不限速）"
        ),
    ) else {
        return;
    };

    match size_parse::parse_rate(&input) {
        Ok(v) => {
            settings.update(|s| s.upload_limit_bytes_per_sec = v);
            if v == 0 {
                info!("发送限速：不限速");
            } else {
                info!("发送限速：{} 字节/秒", v);
            }
        }
        Err(e) => dialog::show_info("ClipSync 设置未生效", &format!("{e}")),
    }
}

/// 弹框修改同步监听端口。
///
/// 端口与其它设置不同：**改了要重启才生效**——监听套接字在启动时就绑好了，
/// 运行中换端口意味着断开所有连接重新监听，还要让对端重新学到新端口。
/// 与其做一套半可靠的热切换，不如如实告诉用户重启一下。
fn prompt_listen_port(settings: &config::SettingsHandle) {
    let current = settings.snapshot().listen_port;
    let Some(input) = dialog::prompt(
        "ClipSync 同步端口",
        &format!("当前：{current}\n\n输入新的端口（1024–65535）：\n改动将在下次启动时生效。"),
    ) else {
        return;
    };

    match input.trim().parse::<u16>() {
        // 1024 以下是特权端口，普通用户绑不上，提前拦住比让它启动时失败好。
        Ok(p) if p >= 1024 => {
            settings.update(|s| s.listen_port = p);
            info!("同步端口已改为 {p}（重启后生效）");
            dialog::show_info(
                "ClipSync 同步端口",
                &format!("已设为 {p}。\n\n请重启 ClipSync 使其生效，并确认对端也能连到这个端口。"),
            );
        }
        Ok(p) => dialog::show_info(
            "ClipSync 设置未生效",
            &format!("端口 {p} 属于系统保留范围，请填 1024–65535 之间的值。"),
        ),
        Err(_) => dialog::show_info(
            "ClipSync 设置未生效",
            &format!("「{input}」不是有效端口，请填 1024–65535 之间的整数。"),
        ),
    }
}

/// 把字节数写成人能读的形式（弹窗里展示当前值用）。
fn describe_bytes(n: usize) -> String {
    if n == usize::MAX {
        return "不限制".to_string();
    }
    if n >= 1 << 30 {
        format!("{:.2} GiB", n as f64 / (1u64 << 30) as f64)
    } else if n >= 1 << 20 {
        format!("{:.0} MiB", n as f64 / (1u64 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.0} KiB", n as f64 / 1024.0)
    } else {
        format!("{n} 字节")
    }
}

