//! 菜单标签的渲染与设备子菜单的重建。
//!
//! 与 `tray` 的分工：那边跑事件循环，这边决定"菜单上显示什么"。
//!
//! **文案原则**：短、不解释、不吓人。菜单是给人扫一眼的，不是说明书——
//! 需要解释的地方交给点击后的对话框，那里有足够篇幅把话说清楚。
//! 可调的项一律把**当前值写进标签**（`自动取回：100 MiB…`），省掉一层
//! "点进去才知道现在是多少"。

use clipsync_core::{t, tf};

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

/// 自动取回上限的显示值。`usize::MAX` 表示多大都自动取。
///
/// 叫「自动取回」而不是「单次上限」：它管的是**收到的文件多大以内自动拉**，
/// 超过的挂起来等你点一下，而不是"超过就不同步了"。发送侧不受它影响。
pub(crate) fn auto_fetch_label(v: usize) -> String {
    if v == usize::MAX {
        t!("自动取回：不限…", "Auto-fetch: unlimited…").to_string()
    } else {
        tf!("自动取回：{}…", "Auto-fetch: {}…", human_bytes(v as u64))
    }
}

/// 「取回」那一项的标签：说清是什么、有多大。
///
/// 名字用与传输进度同一套的截断（前5…后5），免得一个长文件名把菜单撑宽。
/// 多个文件时只报头一个加个数——列全了既放不下，也不比"3 个"更有用。
pub(crate) fn fetch_label(first_name: &str, count: usize, total: u64) -> String {
    let name = crate::tray::ellipsize_middle(first_name);
    let others = count.saturating_sub(1);
    if count > 1 {
        tf!(
            "取回 {name} 等 {count} 个（{}）",
            "Fetch {name} and {others} more ({})",
            human_bytes(total)
        )
    } else {
        tf!("取回 {name}（{}）", "Fetch {name} ({})", human_bytes(total))
    }
}

/// 发送限速的显示值。`0` 表示不限速。
pub(crate) fn rate_label(v: u64) -> String {
    if v == 0 {
        t!("发送限速：不限…", "Upload limit: unlimited…").to_string()
    } else {
        tf!("发送限速：{}/s…", "Upload limit: {}/s…", human_bytes(v))
    }
}

pub(crate) fn port_label(port: u16) -> String {
    tf!("同步端口：{port}…", "Sync port: {port}…")
}

/// 「显示配对码…」那一项的标签。
///
/// 会话进行中时把码和剩余秒数直接写在菜单上：这是唯一能**实时**更新的地方
/// ——弹窗是系统原生模态窗，显示出来文字就定死了，而托盘循环本来就每 200ms
/// 转一圈。用户想知道"还来得及吗"，扫一眼菜单即可，不必再把窗口调出来。
///
/// 分与秒都写出来（`1:23`）而不是纯秒数（`83 秒`）：三位数的秒读起来要在
/// 心里除一遍，而这一栏的宽度也会跟着位数抖。
pub(crate) fn pairing_label(live: Option<(&str, u64)>) -> String {
    match live {
        Some((code, secs)) => tf!(
            "配对码 {code} · 剩 {}:{:02}",
            "Code {code} · {}:{:02} left",
            secs / 60,
            secs % 60
        ),
        None => t!("显示配对码…", "Show pairing code…").to_string(),
    }
}

/// 「语言」那一项的标签，写出当前生效的语言。
///
/// 用各语言的**自称**（中文 / English）而不是当前界面语言里的叫法：英文界面
/// 下写 "Chinese" 对只认中文的人毫无用处，而这一项恰恰是给"看不懂当前界面"
/// 的人找的。
pub(crate) fn language_label() -> String {
    let name = match clipsync_core::i18n::current() {
        clipsync_core::Lang::Zh => "中文",
        clipsync_core::Lang::English => "English",
    };
    tf!("语言：{name}…", "Language: {name}…")
}

/// 设备子菜单里「退出设备组」那一项占用的假 device id。
///
/// 真实 device id 是十六进制串，永远不会等于它，所以拿它做哨兵不会撞车；
/// 换来的是子菜单仍只需返回一张 (菜单项, id) 表，不必为一个按钮再开一路。
pub(super) const LEAVE_GROUP_ID: &str = "\u{1}leave-group";

/// 重建「已配对设备」子菜单。
///
/// 设备列表在运行期会变（配对、被移出），而菜单项是构建时创建的，所以每次
/// 变化都要整体重来一遍。返回新的 (菜单项, device id) 映射供点击时反查。
///
/// 列表为空时放一个禁用的提示项而不是留空白——空子菜单在两个平台上都显示为
/// 一个什么都没有的小方块，看着像坏了。
pub(super) fn rebuild_peer_menu(
    menu: &tray_icon::menu::Submenu,
    peers: &[TrayPeer],
) -> anyhow::Result<Vec<(tray_icon::menu::MenuItem, String)>> {
    use tray_icon::menu::{MenuItem, PredefinedMenuItem};

    // 整体清空再重建。逐项对照着删既啰嗦又容易漏——分隔符不在映射表里，
    // 漏删就会一轮叠一轮。
    while menu.remove_at(0).is_some() {}

    let mut mapping = Vec::with_capacity(peers.len() + 1);
    if peers.is_empty() {
        let empty = MenuItem::new(t!("（尚未配对）", "(no paired devices)"), false, None);
        menu.append(&empty)
            .map_err(|e| anyhow::anyhow!("构建设备子菜单失败: {e}"))?;
        return Ok(mapping);
    }

    for p in peers {
        // ● 在线 / ○ 离线，一眼看出哪台连着。不在标签里写"移出"——
        // 那是点击后确认框的事，菜单只负责列出设备。
        //
        // 引荐来的标出引荐人：那台设备不是你亲手加的，信任是从别处传递
        // 过来的，不标出来等于把这件事藏起来。
        let label = match &p.introduced_by {
            Some(by) => tf!(
                "{} {}（经 {by}）",
                "{} {} (via {by})",
                if p.online { '●' } else { '○' },
                p.name
            ),
            None => format!("{} {}", if p.online { '●' } else { '○' }, p.name),
        };
        let item = MenuItem::new(label, true, None);
        menu.append(&item)
            .map_err(|e| anyhow::anyhow!("构建设备子菜单失败: {e}"))?;
        mapping.push((item, p.device.clone()));
    }

    // 两件事分开摆：点设备是"把它移出组"，点这里是"我自己退出"。
    let sep = PredefinedMenuItem::separator();
    let leave = MenuItem::new(t!("退出设备组…", "Leave device group…"), true, None);
    menu.append(&sep)
        .and_then(|_| menu.append(&leave))
        .map_err(|e| anyhow::anyhow!("构建设备子菜单失败: {e}"))?;
    mapping.push((leave, LEAVE_GROUP_ID.to_string()));

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
        assert_eq!(auto_fetch_label(100 * 1024 * 1024), "自动取回：100 MiB…");
        assert_eq!(auto_fetch_label(usize::MAX), "自动取回：不限…");
        assert_eq!(rate_label(0), "发送限速：不限…");
        assert_eq!(rate_label(10 * 1024 * 1024), "发送限速：10 MiB/s…");
        assert_eq!(port_label(47684), "同步端口：47684…");
    }

    /// 「取回」那一项要说清是什么、几个、多大。
    ///
    /// 长文件名会把菜单撑得很宽（macOS 尤其明显），所以走与传输进度同一套的
    /// 定长截断。
    #[test]
    fn fetch_label_says_what_and_how_big() {
        assert_eq!(
            fetch_label("报告.zip", 1, 4_500_000_000),
            "取回 报告.zip（4.2 GiB）"
        );
        assert_eq!(
            fetch_label("报告.zip", 3, 4_500_000_000),
            "取回 报告.zip 等 3 个（4.2 GiB）"
        );
        // 超长名字截断，菜单不至于被撑宽。
        let long = fetch_label("IMG_20260807_143052_HDR_Portrait_Final.heic", 1, 1 << 20);
        assert!(long.contains('…'), "长名字该截断：{long}");
        assert!(long.chars().count() < 30, "截断后应足够短：{long}");
    }

    /// 英文界面下标签必须真的变成英文。
    ///
    /// 光有 `t!` 宏不代表它接上了——漏写一处的表现是那一项**静默**保持中文，
    /// 而开发者多半在中文系统上开发，永远不会看到。这里逐项比对两种语言的
    /// 产物，顺便锁住"两边都不为空、且确实不同"。
    #[test]
    fn labels_are_translated() {
        use clipsync_core::Lang;

        let cases: Vec<(String, String)> = crate::language::with_lang(Lang::English, || {
            vec![
                (auto_fetch_label(100 << 20), "Auto-fetch: 100 MiB…".into()),
                (
                    auto_fetch_label(usize::MAX),
                    "Auto-fetch: unlimited…".into(),
                ),
                (rate_label(0), "Upload limit: unlimited…".into()),
                (rate_label(10 << 20), "Upload limit: 10 MiB/s…".into()),
                (port_label(47684), "Sync port: 47684…".into()),
                (pairing_label(None), "Show pairing code…".into()),
                (
                    pairing_label(Some(("1234", 167))),
                    "Code 1234 · 2:47 left".into(),
                ),
                (
                    fetch_label("report.zip", 1, 4_500_000_000),
                    "Fetch report.zip (4.2 GiB)".into(),
                ),
                // 中文说「等 3 个」是总数，英文说「and 2 more」是余数——
                // 直译会把数量说错一个。
                (
                    fetch_label("report.zip", 3, 4_500_000_000),
                    "Fetch report.zip and 2 more (4.2 GiB)".into(),
                ),
            ]
        });

        for (got, want) in cases {
            assert_eq!(got, want);
        }
    }

    /// 配对码那一项：没会话时是入口，有会话时是实时倒计时。
    #[test]
    fn pairing_label_switches_between_entry_and_countdown() {
        assert_eq!(pairing_label(None), "显示配对码…");
        assert_eq!(pairing_label(Some(("1234", 167))), "配对码 1234 · 剩 2:47");
        // 秒要补零，否则 `2:7` 读起来像 2 分 7 秒还是 2 分 70 秒都说不准。
        assert_eq!(pairing_label(Some(("0042", 7))), "配对码 0042 · 剩 0:07");
        assert_eq!(pairing_label(Some(("1234", 180))), "配对码 1234 · 剩 3:00");
    }
}
