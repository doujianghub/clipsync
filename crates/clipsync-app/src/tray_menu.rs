//! 菜单标签的渲染与设备子菜单的重建。
//!
//! 与 `tray` 的分工：那边跑事件循环，这边决定"菜单上显示什么"。
//!
//! **文案原则**：短、不解释、不吓人。菜单是给人扫一眼的，不是说明书——
//! 需要解释的地方交给点击后的对话框，那里有足够篇幅把话说清楚。
//! 可调的项一律把**当前值写进标签**（`单次上限：100 MiB…`），省掉一层
//! "点进去才知道现在是多少"。

use super::TrayPeer;

/// 把字节数写成人能读的形式。
///
/// 只挑最合适的那个单位，整数倍时不带小数——"100 MiB" 比 "100.0 MiB" 干净。
pub(crate) fn human_bytes(n: u64) -> String {
    const UNITS: &[(&str, u64)] = &[("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    for (unit, size) in UNITS {
        if n >= *size {
            let v = n as f64 / *size as f64;
            return if v.fract().abs() < 0.05 {
                format!("{v:.0} {unit}")
            } else {
                format!("{v:.1} {unit}")
            };
        }
    }
    format!("{n} B")
}

/// 单次上限的显示值。`usize::MAX` 表示不限制。
pub(crate) fn max_bytes_label(v: usize) -> String {
    if v == usize::MAX {
        "单次上限：不限…".to_string()
    } else {
        format!("单次上限：{}…", human_bytes(v as u64))
    }
}

/// 发送限速的显示值。`0` 表示不限速。
pub(crate) fn rate_label(v: u64) -> String {
    if v == 0 {
        "发送限速：不限…".to_string()
    } else {
        format!("发送限速：{}/s…", human_bytes(v))
    }
}

pub(crate) fn port_label(port: u16) -> String {
    format!("同步端口：{port}…")
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
    while menu.items().len() > old.len().min(menu.items().len()) && !menu.items().is_empty() {
        if menu.remove_at(0).is_none() {
            break;
        }
    }

    let mut mapping = Vec::with_capacity(peers.len());
    if peers.is_empty() {
        let empty = MenuItem::new("（尚未配对）", false, None);
        menu.append(&empty)
            .map_err(|e| anyhow::anyhow!("构建设备子菜单失败: {e}"))?;
        return Ok(mapping);
    }

    for p in peers {
        // ● 在线 / ○ 离线，一眼看出哪台连着。不在标签里写"解除配对"——
        // 那是点击后确认框的事，菜单只负责列出设备。
        let item = MenuItem::new(
            format!("{} {}", if p.online { '●' } else { '○' }, p.name),
            true,
            None,
        );
        menu.append(&item)
            .map_err(|e| anyhow::anyhow!("构建设备子菜单失败: {e}"))?;
        mapping.push((item, p.device.clone()));
    }
    Ok(mapping)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_picks_the_right_unit() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(100 * 1024 * 1024), "100 MiB");
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2 GiB");
        // 整数倍不带小数位。
        assert!(!human_bytes(1 << 20).contains('.'));
        // 非整数倍保留一位，够看又不啰嗦。
        assert_eq!(human_bytes(1536 * 1024 * 1024), "1.5 GiB");
    }

    /// 标签要把当前值写进去——省掉"点进去才知道现在是多少"这一步。
    #[test]
    fn labels_carry_the_current_value() {
        assert_eq!(max_bytes_label(100 * 1024 * 1024), "单次上限：100 MiB…");
        assert_eq!(max_bytes_label(usize::MAX), "单次上限：不限…");
        assert_eq!(rate_label(0), "发送限速：不限…");
        assert_eq!(rate_label(10 * 1024 * 1024), "发送限速：10 MiB/s…");
        assert_eq!(port_label(47684), "同步端口：47684…");
    }
}
