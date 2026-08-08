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
///
/// 参数确实多，但它们是一组彼此无关的依赖——打包成结构体只是把同样的字段挪
/// 个地方声明，调用处并不会因此更清楚，反而多出一个只用一次的类型。
#[allow(clippy::too_many_arguments)]
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
    let pairing_for_version = pairing.clone();
    let pairing_for_code = pairing.clone();
    let status_for_cb = status.clone();
    let hub_for_cb = pairing.hub.clone();
    let running_for_cb = running.clone();
    let settings_for_cb = settings.clone();
    let settings_for_read = settings.clone();
    let settings_for_version = settings.clone();

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
                    pairing_ui::host_pairing_interactive(
                        &dir, &identity, &name, sync_port, &pairing,
                    );
                });
                true
            }
            tray::TrayAction::EnterPairingCode => {
                let dir = dir.clone();
                let identity = identity.clone();
                let name = device_name.clone();
                let pairing = pairing.clone();
                std::thread::spawn(move || {
                    pairing_ui::join_by_code_interactive(
                        &dir, &identity, &name, sync_port, &pairing,
                    );
                });
                true
            }
            tray::TrayAction::NewPairingCode => {
                // 只置一个标志，正在等待的那一轮自己会收摊并开新一轮
                // （见 `host_pairing_interactive` 的循环）。落空时它会说明原因，
                // 而弹窗一律不能占着托盘事件循环。
                let pairing = pairing.clone();
                std::thread::spawn(move || pairing_ui::new_code_from_tray(&pairing));
                true
            }
            tray::TrayAction::FetchPending => {
                // 只管转发。"对方在不在线"、"还提不提供这份内容"都由中枢判定
                // 并解释——它才是唯一知道连接实况的地方，两处各判一次迟早会
                // 说出两套不一样的话。
                hub_for_cb.send(crate::hub::HubEvent::FetchPending);
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
            tray::TrayAction::PromptAutoFetch => {
                let s = settings_for_cb.clone();
                std::thread::spawn(move || prompt_auto_fetch(&s));
                true
            }
            tray::TrayAction::PromptUploadLimit => {
                let s = settings_for_cb.clone();
                std::thread::spawn(move || prompt_upload_limit(&s));
                true
            }
            tray::TrayAction::PromptLanguage => {
                let s = settings_for_cb.clone();
                std::thread::spawn(move || prompt_language(&s));
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
                        &format!(
                            "日志位于：\n{}\n\n（自动打开失败：{e}）",
                            log_control.dir().display()
                        ),
                    );
                }
                true
            }
            tray::TrayAction::RemovePeer(device_id) => {
                // 确认框会阻塞到用户点掉，丢到后台线程免得卡住托盘。
                let pairing = pairing.clone();
                std::thread::spawn(move || {
                    pairing_ui::remove_peer_interactive(&pairing, &device_id)
                });
                true
            }
            tray::TrayAction::LeaveGroup => {
                let pairing = pairing.clone();
                std::thread::spawn(move || pairing_ui::leave_group_interactive(&pairing));
                true
            }
        }),
        current_settings: Box::new(move || {
            let s = settings_for_read.snapshot();
            tray::TraySettings {
                sync_images: s.allow_image,
                sync_files: s.allow_files,
                compress: s.compress_transfers,
                auto_fetch_bytes: s.auto_fetch_bytes,
                upload_limit: s.upload_limit_bytes_per_sec,
                listen_port: s.listen_port,
                verbose_log: s.verbose_log,
            }
        }),
        settings_version: Box::new(move || settings_for_version.version()),
        current_peers: Box::new(move || pairing_for_peers.peers_for_tray()),
        peers_version: Box::new(move || pairing_for_version.known.version()),
        live_pairing_code: Box::new(move || {
            pairing_for_code
                .host_slot
                .live()
                // 向上取整：还剩 0.3 秒时写「剩 0:00」看着像已经没了，而这时
                // 码其实还能用。宁可多显示一秒。
                .map(|l| (l.code, l.remaining.as_millis().div_ceil(1000) as u64))
        }),
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

/// 托盘里点某台设备 → 确认 → 把它移出设备组。
/// 常用档 + 自定义：先让用户点选，选了「自定义…」再弹输入框。
///
/// **为什么不直接弹输入框**：选常用值这件事，点一下本来就比打字省事，
/// 尤其是 `104857600` 这种数。而把档位摆进菜单又会让一级臃肿——所以
/// 菜单只留一行（标签带当前值），选择放进点击后的对话框，两头都不牺牲。
///
/// 输入非法时**再弹一次说明**而不是静默忽略：用户刚打完一串字，什么反馈
/// 都没有只会让人以为程序坏了。
fn pick_setting<T: Copy>(
    settings: &config::SettingsHandle,
    title: &str,
    current: &str,
    presets: &[(&str, T)],
    custom_hint: &str,
    parse: impl Fn(&str) -> anyhow::Result<T>,
    apply: impl Fn(&mut config::Settings, T),
) {
    let mut items: Vec<String> = presets.iter().map(|(label, _)| label.to_string()).collect();
    items.push("自定义…".to_string());

    let Some(idx) = dialog::choose(title, &format!("当前：{current}"), &items) else {
        return; // 用户取消
    };

    if let Some((label, value)) = presets.get(idx) {
        let v = *value;
        settings.update(|s| apply(s, v));
        info!("{title}已设为 {label}");
        return;
    }

    // 选了「自定义…」。
    let Some(input) = dialog::prompt(title, custom_hint) else {
        return;
    };
    match parse(&input) {
        Ok(v) => {
            settings.update(|s| apply(s, v));
            info!("{title}已更新");
        }
        Err(e) => dialog::show_info(title, &format!("{e}")),
    }
}

/// 自动取回上限的常用档。`usize::MAX` 表示多大都自动拉。
///
/// 档位排得密一些是有意的：「自定义…」要弹文本输入框，而输入框是目前唯一
/// 还依赖 PowerShell 子进程的路径，在某些 Windows 上并不可靠。档位覆盖得越
/// 全，需要走那条路的人越少。
const AUTO_FETCH_PRESETS: &[(&str, usize)] = &[
    ("10 MiB", 10 << 20),
    ("50 MiB", 50 << 20),
    ("100 MiB（默认）", 100 << 20),
    ("200 MiB", 200 << 20),
    ("500 MiB", 500 << 20),
    ("1 GiB", 1 << 30),
    ("2 GiB", 2 << 30),
    ("不限", usize::MAX),
];

/// 发送限速的常用档。`0` 表示不限。
const RATE_PRESETS: &[(&str, u64)] = &[
    ("不限（默认）", 0),
    ("2 MB/s", 2_000_000),
    ("5 MB/s", 5_000_000),
    ("10 MB/s", 10_000_000),
    ("20 MB/s", 20_000_000),
    ("50 MB/s", 50_000_000),
    ("100 MB/s", 100_000_000),
];

fn prompt_auto_fetch(settings: &config::SettingsHandle) {
    let cur = settings.snapshot().auto_fetch_bytes;
    let shown = if cur == usize::MAX {
        "不限".to_string()
    } else {
        tray::human_bytes(cur as u64)
    };
    pick_setting(
        settings,
        "自动取回上限",
        &shown,
        AUTO_FETCH_PRESETS,
        "收到的文件多大以内自动拉取，超过的挂起等你点「取回」。\n\
         输入大小，例如 500MB、1.5GiB；填 0 表示多大都自动拉",
        |s| {
            // 0 在这里表示"不限"，而不是"一个字节都不许传"。
            let v = size_parse::parse_byte_size(s)?;
            Ok(if v == 0 { usize::MAX } else { v as usize })
        },
        |s, v| s.auto_fetch_bytes = v,
    );
}

/// 选择界面语言。
///
/// 用选择框而不是 [`pick_setting`] 那套输入框：语言只有三个固定选项，没有
/// "自定义"的余地，让人打字反而是在制造出错的机会。
///
/// **选项名一律用各语言的自称**（中文 / English），不随当前界面语言翻译——
/// 这一项恰恰是给"看不懂当前界面"的人找的，在英文界面里把中文写成
/// "Chinese" 对只认中文的人毫无帮助。
fn prompt_language(settings: &config::SettingsHandle) {
    use crate::language::LangPref;
    use clipsync_core::Lang;

    const CHOICES: [LangPref; 3] = [
        LangPref::Auto,
        LangPref::Fixed(Lang::Zh),
        LangPref::Fixed(Lang::English),
    ];

    let current = LangPref::parse(&settings.snapshot().language);
    let items: Vec<String> = CHOICES
        .iter()
        .map(|p| {
            let name = match p {
                LangPref::Auto => clipsync_core::t!("跟随系统", "Follow system"),
                LangPref::Fixed(Lang::Zh) => "中文",
                LangPref::Fixed(Lang::English) => "English",
            };
            // 标出当前项：选择框本身不显示"现在是哪个"，没有它就得靠记忆。
            if *p == current {
                format!("{name} ✓")
            } else {
                name.to_string()
            }
        })
        .collect();

    let picked = match dialog::choose(
        clipsync_core::t!("界面语言", "Language"),
        clipsync_core::t!(
            "选「跟随系统」则随系统语言自动切换。改完立即生效，无需重启。",
            "\"Follow system\" tracks your system language. Takes effect immediately."
        ),
        &items,
    ) {
        Some(i) => CHOICES[i],
        None => return,
    };

    // 先落盘再生效：反过来的话，写设置失败会留下"这次看着变了、重启又回去"
    // 的状态，比干脆没变更让人困惑。
    settings.update(|s| s.language = picked.as_str().to_string());
    let now = crate::language::apply(picked);
    info!("界面语言已切换为 {}", now.as_str());
}

fn prompt_upload_limit(settings: &config::SettingsHandle) {
    let cur = settings.snapshot().upload_limit_bytes_per_sec;
    let shown = if cur == 0 {
        "不限".to_string()
    } else {
        format!("{}/s", tray::human_bytes(cur))
    };
    pick_setting(
        settings,
        "发送限速",
        &shown,
        RATE_PRESETS,
        "输入速率，例如 10MB/s、20mbps；填 0 表示不限",
        size_parse::parse_rate,
        |s, v| s.upload_limit_bytes_per_sec = v,
    );
}

/// 端口没有"常用档"可言，直接输入。
///
/// 它与其它设置还有一点不同：**改了要重启才生效**。监听套接字在启动时就
/// 绑好了，运行中换端口意味着断开所有连接重新监听，还要让对端重新学到新
/// 端口。与其做一套半可靠的热切换，不如如实告诉用户重启一下。
fn prompt_listen_port(settings: &config::SettingsHandle) {
    let cur = settings.snapshot().listen_port;
    let Some(input) = dialog::prompt(
        "同步端口",
        &format!("当前：{cur}\n\n填 1024–65535；重启后生效"),
    ) else {
        return;
    };
    match input.trim().parse::<u16>() {
        // 1024 以下是特权端口，普通用户绑不上，提前拦住比启动时才失败好。
        Ok(p) if p >= 1024 => {
            settings.update(|s| s.listen_port = p);
            info!("同步端口已设为 {p}（重启后生效）");
        }
        Ok(p) => dialog::show_info(
            "同步端口",
            &format!("{p} 属于系统保留范围，请填 1024–65535"),
        ),
        Err(_) => dialog::show_info("同步端口", &format!("「{input}」不是有效端口")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 档位的标签与数值必须对得上。
    ///
    /// 这类常量表最容易手滑写错数量级（把 MiB 写成 MB、少一个零），而错了
    /// 之后界面显示的是"100 MiB"、实际生效的却是别的数——用户没法察觉。
    ///
    /// **按标签查而不是按下标**：加减档位是常事，按下标写死的断言会在每次
    /// 增删时误报，最后只能靠改测试来"修"，久而久之就没人信它了。
    #[test]
    fn presets_match_their_labels() {
        let by_label = |label: &str| -> usize {
            AUTO_FETCH_PRESETS
                .iter()
                .find(|(l, _)| *l == label)
                .unwrap_or_else(|| panic!("找不到档位 {label}"))
                .1
        };
        assert_eq!(by_label("10 MiB"), 10 * 1024 * 1024);
        assert_eq!(by_label("100 MiB（默认）"), 100 * 1024 * 1024);
        assert_eq!(by_label("500 MiB"), 500 * 1024 * 1024);
        assert_eq!(by_label("1 GiB"), 1024 * 1024 * 1024);
        assert_eq!(by_label("2 GiB"), 2 * 1024 * 1024 * 1024);
        assert_eq!(by_label("不限"), usize::MAX);

        // 速率按 1000 进制（网络惯例，与 MB/s 的通常含义一致）。
        let rate = |label: &str| -> u64 {
            RATE_PRESETS
                .iter()
                .find(|(l, _)| *l == label)
                .unwrap_or_else(|| panic!("找不到限速档 {label}"))
                .1
        };
        assert_eq!(rate("不限（默认）"), 0);
        assert_eq!(rate("10 MB/s"), 10_000_000);
        assert_eq!(rate("50 MB/s"), 50_000_000);
        assert_eq!(rate("100 MB/s"), 100_000_000);
    }

    /// 档位应当递增，否则列表读起来很怪。
    #[test]
    fn presets_are_ordered() {
        let sizes: Vec<usize> = AUTO_FETCH_PRESETS.iter().map(|(_, v)| *v).collect();
        assert!(
            sizes.windows(2).all(|w| w[0] < w[1]),
            "上限档应递增: {sizes:?}"
        );

        // 限速的「不限」排第一（0 表示不限，语义上是最大），其余递增。
        let rates: Vec<u64> = RATE_PRESETS[1..].iter().map(|(_, v)| *v).collect();
        assert!(
            rates.windows(2).all(|w| w[0] < w[1]),
            "限速档应递增: {rates:?}"
        );
    }
}
