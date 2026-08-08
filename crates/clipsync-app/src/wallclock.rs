//! 本地墙钟时刻，只为把「有效至 21:47:30」写进弹窗。
//!
//! **为什么需要绝对时刻**：弹窗是系统原生的模态窗（macOS 是 `osascript` 子
//! 进程，Windows 是 TaskDialog），显示出来就定死了，改不了文字。写「还有 2 分
//! 47 秒」的话，这个数字从显示的那一刻起就开始撒谎；写死到某个时刻则永远为
//! 真。实时倒计时在托盘菜单里——那儿本来就每 200ms 转一圈。
//!
//! **为什么不引时间库**：需要的只是"时分秒"。`chrono`/`time` 为此多背一整套
//! 日历与时区数据库，而系统本来就有一个现成的本地时间接口。

use std::time::Duration;

/// 一天的秒数。
const DAY: i64 = 86_400;

/// `d` 之后的本地时刻，格式 `HH:MM:SS`；取不到系统时间时返回 `None`。
///
/// 跨午夜按天回绕。**不考虑夏令时跳变**：调用它的场景是几分钟内的到期时刻，
/// 一年一次的跳变正好落在那几分钟里的概率可以忽略，落到了也只是显示的时刻
/// 差一小时，不影响任何判断。
pub fn hms_after(d: Duration) -> Option<String> {
    let sod = local_secs_of_day()?;
    Some(fmt_hms(sod + d.as_secs() as i64))
}

/// 把"当天第几秒"排成 `HH:MM:SS`，超过一天的部分回绕。
///
/// 单独成纯函数是为了能不碰系统时钟就把回绕与补零测掉。
fn fmt_hms(secs: i64) -> String {
    let s = secs.rem_euclid(DAY);
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// 当前本地时间是当天的第几秒。
#[cfg(unix)]
fn local_secs_of_day() -> Option<i64> {
    // SAFETY: `time(NULL)` 只返回值不写内存；`localtime_r` 写入我们自己的
    // `tm`，且返回空指针时我们不读它。
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return None;
        }
        Some(tm.tm_hour as i64 * 3600 + tm.tm_min as i64 * 60 + tm.tm_sec as i64)
    }
}

#[cfg(windows)]
fn local_secs_of_day() -> Option<i64> {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: GetLocalTime 只填充我们提供的结构体，无失败路径。
    unsafe { GetLocalTime(&mut st) };
    Some(st.wHour as i64 * 3600 + st.wMinute as i64 * 60 + st.wSecond as i64)
}

#[cfg(not(any(unix, windows)))]
fn local_secs_of_day() -> Option<i64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 补零与进位——时刻要能一眼读，`9:5:3` 那样对不齐。
    #[test]
    fn formats_with_leading_zeros() {
        assert_eq!(fmt_hms(0), "00:00:00");
        assert_eq!(fmt_hms(9 * 3600 + 5 * 60 + 3), "09:05:03");
        assert_eq!(fmt_hms(23 * 3600 + 59 * 60 + 59), "23:59:59");
    }

    /// 跨午夜要回绕，不能出现 `24:01:00` 这种读不通的时刻。
    #[test]
    fn wraps_past_midnight() {
        assert_eq!(fmt_hms(DAY), "00:00:00");
        assert_eq!(fmt_hms(DAY + 61), "00:01:01");
        // 负数（时钟被往回调）也不能 panic 或给出负时刻。
        assert_eq!(fmt_hms(-1), "23:59:59");
    }

    /// 真的问一次系统时钟：格式对、数值在合法范围内。
    ///
    /// 不断言具体时刻（那取决于跑测试的时候），只断言"确实拿到了一个像样的
    /// 本地时刻"——这条守的是平台分支有没有接对。
    #[test]
    fn system_clock_gives_a_sane_local_time() {
        let Some(s) = hms_after(Duration::ZERO) else {
            return; // 本平台无实现，跳过
        };
        let parts: Vec<i64> = s.split(':').map(|p| p.parse().unwrap()).collect();
        assert_eq!(parts.len(), 3, "格式应为 HH:MM:SS，实际 {s}");
        assert!((0..24).contains(&parts[0]), "小时越界: {s}");
        assert!((0..60).contains(&parts[1]), "分钟越界: {s}");
        assert!((0..60).contains(&parts[2]), "秒越界: {s}");

        // 三分钟后的时刻必须与现在不同（除非正好整点回绕撞上，概率为零）。
        let later = hms_after(Duration::from_secs(180)).unwrap();
        assert_ne!(later, s, "加了 3 分钟却得到同一时刻");
    }
}
