//! 托盘上那几行字的渲染。
//!
//! 与 `tray_status` 的分工：那边存状态，这边把状态排成人能一眼读懂的一行。
//! 分开是因为这里的每个决定都是**排版**问题——截多长、换不换行、单位写不写
//! 两遍——与"谁在线、传到哪了"完全正交，混在一起两边都难读。

use super::ProgressState;

/// 界面上文件名保留的头尾字符数：`前5…后5`。
///
/// 名字长到看不完时，真正有辨识度的就是开头和结尾——结尾还带着扩展名与
/// 序号。中间那一大段（日期、参数、哈希）反而是最不需要看清的部分。
/// 11 个字符（5+1+5）足够认出是哪个文件，也让托盘那行不至于抖。
pub(super) const NAME_HEAD: usize = 5;
pub(super) const NAME_TAIL: usize = 5;

/// Windows 托盘提示的硬上限：`NOTIFYICONDATA.szTip` 是 64 个 UTF-16 单元
/// （63 个字符 + 结尾的 0），超出部分被系统直接切掉，不换行也不省略。
///
/// 实机截图里正好断在第 64 个字符上，后半截连百分比都看不到。注意这个限制
/// 数的是**字符数**（汉字也只算一个 UTF-16 单元），与菜单那行能占多宽是两回
/// 事——这也是提示与菜单项必须分开生成的原因。
pub(super) const TOOLTIP_MAX_CHARS: usize = 63;

/// 把字符串硬塞进 `max` 个字符，超了就尾部省略。
///
/// 最后一道保险。正常路径上文案已经短于上限，走到这里说明哪里算漏了——
/// 宁可自己带个省略号，也别让系统在半截字上切一刀。
pub(super) fn fit_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

/// 从中间省略，保留固定的头尾。
///
/// 不按"总长度预算"分配头尾，而是写死 `前5…后5`：预算式分配会让名字长度
/// 随剩余空间浮动，托盘那行的宽度跟着变，在换行临界点上就会一会儿一行、
/// 一会儿两行地抖——实机上就是这个毛病。定长才稳。
pub(crate) fn ellipsize_middle(s: &str) -> String {
    let n = s.chars().count();
    if n <= NAME_HEAD + 1 + NAME_TAIL {
        return s.to_string();
    }
    let head: String = s.chars().take(NAME_HEAD).collect();
    let tail: String = s.chars().skip(n - NAME_TAIL).collect();
    format!("{head}…{tail}")
}

/// `已传 / 总量`。单位相同就只写一次：`3.9 / 9.3 GiB` 比
/// `3.9 GiB / 9.3 GiB` 短五列，也更好读——重复的单位不带任何信息。
pub(super) fn bytes_pair(done: u64, total: u64) -> String {
    let d = crate::tray::human_bytes(done);
    let t = crate::tray::human_bytes(total);
    match (d.rsplit_once(' '), t.rsplit_once(' ')) {
        (Some((dv, du)), Some((_, tu))) if du == tu => format!("{dv} / {t}"),
        _ => format!("{d} / {t}"),
    }
}

/// 把进度渲染成一行人话。托盘与日志共用同一套格式。
///
/// `shorten` 只影响文件名：托盘要一眼扫过、且 Windows 的托盘提示有硬上限，
/// 太长会被系统直接切掉；日志要完整，那是事后排查唯一的凭据。
pub(super) fn describe(st: &ProgressState, shorten: bool) -> String {
    {
        let dir = if st.p.sending { "发送" } else { "接收" };
        let name = if shorten {
            ellipsize_middle(&st.p.name)
        } else {
            st.p.name.clone()
        };
        let pct = if st.p.total > 0 {
            st.p.done.saturating_mul(100) / st.p.total
        } else {
            0
        };

        // 速度要等锚点之后确实有字节流过才显示。刚开头就报一个由极短时间
        // 算出的数字，往往是个离谱的大值，反而不如不显示。
        let moved = st.p.done.saturating_sub(st.since_bytes);
        let secs = st.since.elapsed().as_secs_f64();
        if moved > 0 && secs >= 0.5 {
            let rate = (moved as f64 / secs) as u64;
            format!(
                "{dir} {name} {pct}%（{}，{}/s）",
                bytes_pair(st.p.done, st.p.total),
                crate::tray::human_bytes(rate)
            )
        } else {
            format!("{dir} {name} {pct}%")
        }
    }
}
