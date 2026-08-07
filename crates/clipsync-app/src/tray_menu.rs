//! 菜单项的取值档位、标签渲染与设备子菜单的重建。
//!
//! 与 `tray` 的分工：那边跑事件循环，这边决定"菜单上显示什么"。

use super::TrayPeer;

/// 单次大小上限的预设档。`usize::MAX` 表示不限制。
///
/// 预设档覆盖常见场景，档位之外由子菜单末尾的「自定义…」承接——它会弹一个
/// 输入框，接受 `500MB`、`1.5GiB` 这类写法。
///
/// （早先这里写的是"托盘菜单没有输入框，需要精确值的用户可直接改
/// `settings.json`"。自从配对流程引入 `dialog::prompt` 后这个前提就不成立了，
/// 而让用户去翻 `~/Library/Application Support/` 手改 JSON 显然不是好答案。）
pub(super) const MAX_BYTES_PRESETS: &[(&str, usize)] = &[
    ("10 MiB", 10 * 1024 * 1024),
    ("100 MiB（默认）", 100 * 1024 * 1024),
    ("500 MiB", 500 * 1024 * 1024),
    ("2 GiB", 2 * 1024 * 1024 * 1024),
    ("不限制", usize::MAX),
];

/// 发送限速的预设档。`0` 表示不限速。
pub(super) const UPLOAD_LIMIT_PRESETS: &[(&str, u64)] = &[
    ("不限速（默认）", 0),
    ("10 MB/s", 10 * 1000 * 1000),
    ("20 MB/s", 20 * 1000 * 1000),
    ("50 MB/s", 50 * 1000 * 1000),
];


/// 「自定义…」项的标签。当前值不在预设档里时带上实际数值，让用户一眼看出
/// 现在生效的是多少——否则子菜单里一个勾都没有，会显得像没设置过。
pub(super) fn custom_label_bytes(current: usize) -> String {
    if MAX_BYTES_PRESETS.iter().any(|(_, v)| *v == current) {
        "自定义…".to_string()
    } else {
        format!("自定义…（当前 {}）", human_bytes(current))
    }
}

pub(super) fn custom_label_rate(current: u64) -> String {
    if UPLOAD_LIMIT_PRESETS.iter().any(|(_, v)| *v == current) {
        "自定义…".to_string()
    } else {
        format!("自定义…（当前 {}/s）", human_bytes(current as usize))
    }
}

/// 把字节数写成人能读的形式。挑最合适的单位，避免出现 "0.00 GiB" 这种。
pub(super) fn human_bytes(n: usize) -> String {
    if n == usize::MAX {
        return "不限制".to_string();
    }
    const UNITS: &[(&str, usize)] = &[
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
    ];
    for (unit, size) in UNITS {
        if n >= *size {
            let v = n as f64 / *size as f64;
            // 整数倍就不显示小数位，"100 MiB" 比 "100.0 MiB" 干净。
            return if (v.fract()).abs() < 0.05 {
                format!("{:.0} {unit}", v)
            } else {
                format!("{v:.1} {unit}")
            };
        }
    }
    format!("{n} B")
}

/// 重建「已配对设备」子菜单。
///
/// 设备列表在运行期会变（配对、解除配对），而菜单项是构建时创建的，所以每次
/// 变化都要整体重来一遍。返回新的 (菜单项, device id) 映射供点击时反查。
///
/// 列表为空时放一个禁用的提示项而不是留空白——空子菜单在两个平台上都显示为
/// 一个什么都没有的小方块，看着像坏了。
pub(super) fn rebuild_peer_menu(
    menu: &tray_icon::menu::Submenu,
    old: &[(tray_icon::menu::MenuItem, String)],
    peers: &[TrayPeer],
) -> anyhow::Result<Vec<(tray_icon::menu::MenuItem, String)>> {
    use tray_icon::menu::MenuItem;

    for (item, _) in old {
        let _ = menu.remove(item);
    }
    // 上一轮的占位项也要清掉，否则会越堆越多。
    for item in menu.items() {
        let _ = menu.remove_at(0);
        drop(item);
    }

    let mut mapping = Vec::with_capacity(peers.len());
    if peers.is_empty() {
        let empty = MenuItem::new("（尚未配对任何设备）", false, None);
        menu.append(&empty)
            .map_err(|e| anyhow::anyhow!("构建设备子菜单失败: {e}"))?;
        return Ok(mapping);
    }

    for p in peers {
        // ● 在线 / ○ 离线，一眼能看出哪台连着。文案写明点击的后果——
        // 这是个破坏性操作，不能让人以为只是查看详情。
        let label = format!(
            "{} {} — 解除配对",
            if p.online { '●' } else { '○' },
            p.name
        );
        let item = MenuItem::new(label, true, None);
        menu.append(&item)
            .map_err(|e| anyhow::anyhow!("构建设备子菜单失败: {e}"))?;
        mapping.push((item, p.device.clone()));
    }
    Ok(mapping)
}
