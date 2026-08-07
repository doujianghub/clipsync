//! 托盘展示的同步状态。
//!
//! 中枢更新、托盘读取，两边通过这个共享句柄通信。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 最后一次传输活动之后，图标还继续脉冲多久。
///
/// 分块之间本来就有间隙（限速、磁盘、对端处理），太短会让图标一顿一顿的；
/// 太长又会在传完之后还闪半天。1.5 秒是"分块间隙盖得住、传完很快停"的折中。
const TRANSFER_LINGER: Duration = Duration::from_millis(1500);

/// 一次进行中的文件传输，供托盘显示进度。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferProgress {
    /// true 为发出，false 为收进。
    pub sending: bool,
    /// 当前文件名。多文件时是正在传的那一个——逐个传，报总数反而看不出在动。
    pub name: String,
    pub done: u64,
    pub total: u64,
}

/// 进度加上算速度所需的锚点。
struct ProgressState {
    p: TransferProgress,
    /// 速度锚点：从这个时刻的这个字节数算起。
    since: Instant,
    since_bytes: u64,
    /// 上次写进日志的时刻。
    logged: Instant,
}

/// 传输进度写进日志的间隔。
///
/// 实机上一次 18.5 GB 的接收，8 分 19 秒里日志只有每分钟两条地址通告，
/// 中间一片空白——出问题时完全无从判断卡在哪。托盘上有进度，但日志是
/// 事后排查唯一的凭据。10 秒一条，长传输也就几十行。
const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// 速度锚点的重设间隔。
///
/// 不从头平均：传输中途限速变了、网络抖了，平均值会把当前实际速度糊掉，
/// 用户看到的数字和眼前的进度条对不上。每隔几秒重新锚定，显示的就是"最近
/// 这几秒有多快"。
const RATE_WINDOW: Duration = Duration::from_secs(3);

/// 距上次分块是否仍在"算作传输中"的窗口内。
///
/// 单独成纯函数只为能便宜地穷举边界——否则每验一个边界都要真等 1.5 秒。
/// `last == 0` 表示从未传输过。
fn transfer_is_recent(now_ms: u64, last_ms: u64) -> bool {
    last_ms != 0 && now_ms.saturating_sub(last_ms) < TRANSFER_LINGER.as_millis() as u64
}

/// 同步状态，供托盘展示。
#[derive(Debug, Clone, Default)]
pub struct StatusData {
    /// 当前已连接的对端数。
    pub connected: usize,
    /// 已配对设备总数。
    pub paired: usize,
    /// 当前在线的设备 ID 集合，用于在设备列表里标出 ●/○。
    pub connected_ids: std::collections::HashSet<String>,
}

/// 线程安全的状态句柄：中枢更新，托盘读取。
#[derive(Clone)]
pub struct TrayStatus {
    data: Arc<Mutex<StatusData>>,
    paused: Arc<AtomicBool>,
    /// 同步中枢是否已停止工作（异常退出）。
    hub_dead: Arc<AtomicBool>,
    /// 进程启动时刻，配合 `last_transfer` 换算"多久之前"。
    started: Arc<Instant>,
    /// 进行中的传输及其速度锚点。
    progress: Arc<Mutex<Option<ProgressState>>>,
    /// 最后一次文件分块收发距 `started` 的毫秒数；0 表示从未传输过。
    ///
    /// **用"最后活动时刻"而不是"进行中计数"**：计数要在收发两侧各处出口
    /// 精确配对增减，漏掉任何一条错误路径就会永久泄漏，表现是图标一直闪个
    /// 不停——一个只在出错后才显现、且很难查的毛病。时间戳没有这个问题，
    /// 它自己会过期。
    last_transfer: Arc<AtomicU64>,
}

impl TrayStatus {
    pub fn new(paired: usize) -> Self {
        Self {
            data: Arc::new(Mutex::new(StatusData {
                connected: 0,
                paired,
                connected_ids: std::collections::HashSet::new(),
            })),
            paused: Arc::new(AtomicBool::new(false)),
            hub_dead: Arc::new(AtomicBool::new(false)),
            started: Arc::new(Instant::now()),
            last_transfer: Arc::new(AtomicU64::new(0)),
            progress: Arc::new(Mutex::new(None)),
        }
    }

    /// 记一次文件分块的收发并更新进度。收发两侧都调用。
    ///
    /// 每个分块调一次，所以要廉价：一次 `Instant::elapsed`、一次原子写、
    /// 一把无争用的短锁。
    pub fn note_transfer(&self, p: TransferProgress) {
        let ms = self.started.elapsed().as_millis() as u64;
        // 用 max 语义没必要——晚到的写覆盖早到的也只是让脉冲多持续几毫秒。
        self.last_transfer.store(ms.max(1), Ordering::Relaxed);

        let mut g = self.progress.lock().unwrap();
        match g.as_mut() {
            // 同一个文件、同一个方向：只更新进度，锚点按窗口滚动。
            Some(st) if st.p.name == p.name && st.p.sending == p.sending => {
                if st.since.elapsed() >= RATE_WINDOW {
                    st.since = Instant::now();
                    st.since_bytes = st.p.done;
                }
                st.p = p;
                if st.logged.elapsed() >= PROGRESS_LOG_INTERVAL {
                    st.logged = Instant::now();
                    // 日志里保留完整文件名：它是事后排查唯一的凭据，截断等于
                    // 丢信息。托盘那边才需要短——两者格式一致，只差这一点。
                    tracing::info!("{}", describe(st, false));
                }
            }
            // 换文件了：重新锚定，否则新文件的速度会被上一个的平均值污染。
            _ => {
                *g = Some(ProgressState {
                    since_bytes: p.done,
                    p,
                    since: Instant::now(),
                    logged: Instant::now(),
                })
            }
        }
    }

    /// 传输结束（完成、中止、连接断开）——清掉进度，摘要回到常态。
    pub fn clear_transfer(&self) {
        *self.progress.lock().unwrap() = None;
    }

    /// 进度文案，例如 `接收 video.mp4 42% · 8.3 MB/s`；没有传输时为 `None`。
    ///
    /// 与写进日志的是同一份文案（见 [`describe`]），托盘上看到什么、日志里
    /// 就记什么，排查时不必在两套措辞之间对照。
    ///
    /// **以 [`is_transferring`](Self::is_transferring) 为准，而不是靠各处出口
    /// 记得清进度**。传输的结束路径有一大把——传完、被新内容取代、对端
    /// `FileAbort`、解压失败、写盘失败、连接断开——挨个补一句 `clear_transfer`
    /// 迟早会漏，漏了就是托盘上永远挂着一条"接收 42%"。同一个时间戳既驱动
    /// 图标脉冲也驱动这里，1.5 秒没有新分块就自动归位；`clear_transfer` 退化
    /// 成"让它立刻归位"的优化，不再是正确性的前提。
    fn progress_text(&self) -> Option<String> {
        if !self.is_transferring() {
            return None;
        }
        let g = self.progress.lock().unwrap();
        Some(describe(g.as_ref()?, true))
    }
}

/// 进度里文件名允许占的显示宽度（半角为 1，全角/中日韩为 2）。
///
/// 菜单项过长在 macOS 上把整张菜单撑得很宽，在 Windows 上则直接被托盘提示
/// 截断（`NOTIFYICONDATA` 的提示文本有硬上限），后半截连百分比都看不到。
/// 28 列约等于 28 个英文字符或 14 个汉字，足够辨认是哪个文件。
const NAME_MAX_WIDTH: usize = 28;

/// 字符的显示宽度。东亚全角字符占两列。
///
/// 不引入 unicode-width 之类的依赖：这里只需要"别把菜单撑爆"，按区段粗判
/// 足够，判错一两个字符最多让宽度差一列。
fn char_width(c: char) -> usize {
    let u = c as u32;
    let wide = (0x1100..=0x115F).contains(&u)      // 韩文字母
        || (0x2E80..=0xA4CF).contains(&u)          // CJK 部首、假名、汉字
        || (0xAC00..=0xD7A3).contains(&u)          // 韩文音节
        || (0xF900..=0xFAFF).contains(&u)          // CJK 兼容汉字
        || (0xFE30..=0xFE6F).contains(&u)          // 竖排标点
        || (0xFF00..=0xFF60).contains(&u)          // 全角字符
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x1F300..=0x1FAFF).contains(&u); // emoji
    1 + wide as usize
}

fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// 超宽时**从中间**省略：`一个很长的视频文件…part3.mp4`。
///
/// 不从尾部截：尾部截断会把扩展名切掉，只剩"很长很长的名字…"，连是视频
/// 还是压缩包都看不出来。而文件名里最能区分彼此的信息，恰恰常在结尾
/// （序号、日期、清晰度）。
fn ellipsize_middle(s: &str, max_width: usize) -> String {
    if display_width(s) <= max_width {
        return s.to_string();
    }
    // 省略号自身占一列；余下的宽度前六后四分，保住扩展名又不至于头太短。
    let budget = max_width.saturating_sub(1);
    let tail_budget = budget * 4 / 10;
    let head_budget = budget - tail_budget;

    let mut head = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > head_budget {
            break;
        }
        head.push(c);
        w += cw;
    }

    let mut tail: Vec<char> = Vec::new();
    let mut w = 0;
    for c in s.chars().rev() {
        let cw = char_width(c);
        if w + cw > tail_budget {
            break;
        }
        tail.push(c);
        w += cw;
    }
    tail.reverse();

    format!("{head}…{}", tail.into_iter().collect::<String>())
}

/// 把进度渲染成一行人话。托盘与日志共用同一套格式。
///
/// `shorten` 只影响文件名：托盘要一眼扫过、且 Windows 的托盘提示有硬上限，
/// 太长会被系统直接切掉；日志要完整，那是事后排查唯一的凭据。
fn describe(st: &ProgressState, shorten: bool) -> String {
    {
        let dir = if st.p.sending { "发送" } else { "接收" };
        let name = if shorten {
            ellipsize_middle(&st.p.name, NAME_MAX_WIDTH)
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
                "{dir} {name} {pct}%（{} / {}，{}/s）",
                crate::tray::human_bytes(st.p.done),
                crate::tray::human_bytes(st.p.total),
                crate::tray::human_bytes(rate)
            )
        } else {
            format!("{dir} {name} {pct}%")
        }
    }
}

impl TrayStatus {

    /// 眼下是否正在传文件（据此让托盘图标脉冲、摘要报进度）。
    pub fn is_transferring(&self) -> bool {
        transfer_is_recent(
            self.started.elapsed().as_millis() as u64,
            self.last_transfer.load(Ordering::Relaxed),
        )
    }

    /// 更新在线设备集合（同时刷新计数，避免两者脱节）。
    pub fn set_connected_ids(&self, ids: std::collections::HashSet<String>) {
        let mut g = self.data.lock().unwrap();
        g.connected = ids.len();
        g.connected_ids = ids;
    }

    /// 当前在线的设备 ID。
    pub fn connected_devices(&self) -> std::collections::HashSet<String> {
        self.data.lock().unwrap().connected_ids.clone()
    }

    /// 更新已配对设备总数。
    ///
    /// 配对可以在运行期从托盘发起，配完这个数就变了——不更新的话菜单首行
    /// 会一直停在"尚未配对设备"，而同步其实已经在工作了。
    pub fn set_paired(&self, n: usize) {
        self.data.lock().unwrap().paired = n;
    }

    pub fn snapshot(&self) -> StatusData {
        self.data.lock().unwrap().clone()
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    /// 标记同步中枢已停止工作。
    ///
    /// 中枢线程若因意外退出，所有同步都会静默停止，而托盘图标还是绿的、
    /// 菜单还写着"已连接 N 台"——用户根本无从察觉，只会觉得"复制了怎么没
    /// 过去"。宁可明确显示故障，也不要给一个骗人的正常状态。
    pub fn set_hub_dead(&self) {
        self.hub_dead.store(true, Ordering::SeqCst);
    }

    pub fn is_hub_dead(&self) -> bool {
        self.hub_dead.load(Ordering::SeqCst)
    }

    /// 一句话状态描述，用于托盘提示文本。
    pub fn summary(&self) -> String {
        if self.is_hub_dead() {
            return "ClipSync — 同步已停止（请重启程序）".to_string();
        }
        if self.is_paused() {
            return "ClipSync — 已暂停".to_string();
        }
        // 传输中优先报进度：这时候用户最想知道的是"到哪了"，不是连了几台。
        if let Some(p) = self.progress_text() {
            return format!("ClipSync — {p}");
        }
        let s = self.snapshot();
        if s.paired == 0 {
            "ClipSync — 尚未配对设备".to_string()
        } else if s.connected == 0 {
            format!("ClipSync — 未连接（已配对 {} 台）", s.paired)
        } else {
            format!("ClipSync — 已连接 {} / {} 台", s.connected, s.paired)
        }
    }
}

/// `Instant` 没有 `Default`，手写一个等价于 `new(0)` 的实现。
impl Default for TrayStatus {
    fn default() -> Self {
        Self::new(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prog(name: &str, done: u64, total: u64) -> TransferProgress {
        TransferProgress {
            sending: false,
            name: name.into(),
            done,
            total,
        }
    }

    /// 传输中，摘要要报进度而不是"已连接 N 台"——这时用户最想知道的是到哪了。
    #[test]
    fn summary_reports_progress_while_transferring() {
        let s = TrayStatus::new(2);
        s.set_connected_ids(["a".to_string()].into_iter().collect());
        assert!(s.summary().contains("已连接"));

        s.note_transfer(prog("video.mp4", 42, 100));
        let sum = s.summary();
        assert!(sum.contains("接收"), "要说方向，实际 {sum}");
        assert!(sum.contains("video.mp4"), "要说文件名，实际 {sum}");
        assert!(sum.contains("42%"), "要说百分比，实际 {sum}");
    }

    /// 过期判定的边界。
    #[test]
    fn transfer_recency_boundaries() {
        let w = TRANSFER_LINGER.as_millis() as u64;
        assert!(!transfer_is_recent(0, 0), "从未传输过");
        assert!(!transfer_is_recent(9_999, 0), "从未传输过，多久都不算");
        assert!(transfer_is_recent(100, 100), "刚刚就是现在");
        assert!(transfer_is_recent(100 + w - 1, 100), "窗口内");
        assert!(!transfer_is_recent(100 + w, 100), "刚好到窗口边界即过期");
        // 时钟不会倒流，但真倒了也不能 panic 或误判成"很久以前"。
        assert!(transfer_is_recent(50, 100), "负差被 saturating 夹到 0");
    }

    /// 进度必须**自己过期**，不能依赖各处出口记得清。
    ///
    /// 传输的结束路径有一大把——传完、被新内容取代、对端 FileAbort、解压
    /// 失败、写盘失败、连接断开——挨个补一句清理迟早会漏，漏了就是托盘上
    /// 永远挂着一条"接收 42%"。
    ///
    /// 这条测试要真等一个 `TRANSFER_LINGER`（1.5 秒）。值：它验的是"摘要
    /// 确实挂在 `is_transferring` 上"这个联动，而联动正是漏清理时唯一还能
    /// 兜住的东西；边界算术已由上一条免费覆盖。
    #[test]
    fn progress_expires_without_anyone_clearing_it() {
        let s = TrayStatus::new(1);
        s.note_transfer(prog("big.zip", 1, 100));
        assert!(s.summary().contains("big.zip"));

        std::thread::sleep(TRANSFER_LINGER + Duration::from_millis(50));
        let sum = s.summary();
        assert!(!sum.contains("big.zip"), "过期后不该还挂着进度：{sum}");
        assert!(sum.contains("未连接") || sum.contains("已连接") || sum.contains("尚未配对"));
    }

    /// 换文件要重新锚定速度，否则新文件的速率被上一个的平均值污染。
    #[test]
    fn switching_files_reanchors_the_rate() {
        let s = TrayStatus::new(1);
        s.note_transfer(prog("a.bin", 0, 1000));
        s.note_transfer(prog("a.bin", 900, 1000));

        s.note_transfer(prog("b.bin", 0, 1000));
        let g = s.progress.lock().unwrap();
        let st = g.as_ref().unwrap();
        assert_eq!(st.since_bytes, 0, "新文件的速度锚点应从它自己的起点算");
        assert_eq!(st.p.name, "b.bin");
    }

    /// 刚开头不报速度：由极短时间算出的数字往往离谱，不如不显示。
    #[test]
    fn rate_is_withheld_until_it_is_meaningful() {
        let s = TrayStatus::new(1);
        s.note_transfer(prog("x.bin", 500, 1000));
        let sum = s.summary();
        assert!(sum.contains("50%"));
        assert!(!sum.contains("/s"), "刚开头不该报速度：{sum}");
    }

    /// 短名字原样保留，不该无端加省略号。
    #[test]
    fn short_names_pass_through() {
        assert_eq!(ellipsize_middle("a.txt", 28), "a.txt");
        assert_eq!(ellipsize_middle("报告.pdf", 28), "报告.pdf");
        // 正好卡在上限也不截。
        let exact = "a".repeat(28);
        assert_eq!(ellipsize_middle(&exact, 28), exact);
    }

    /// 超宽时从中间省略，且**扩展名必须留着**。
    ///
    /// 从尾部截会切掉扩展名，只剩"很长很长的名字…"——连是视频还是压缩包都
    /// 看不出来；而文件名里最能区分彼此的信息（序号、日期、清晰度）恰恰
    /// 常在结尾。
    #[test]
    fn long_names_keep_head_and_extension() {
        let s = "2026年度第三季度产品发布会现场录像完整版第三部分.mp4";
        let out = ellipsize_middle(s, 28);

        assert!(out.contains('…'), "应当省略：{out}");
        assert!(out.ends_with(".mp4"), "扩展名必须留着：{out}");
        assert!(out.starts_with("2026"), "开头也要留着：{out}");
        assert!(display_width(&out) <= 28, "宽度超了：{out}");
    }

    /// 宽度按显示列算，不是按字符数——否则中文名会把菜单撑到两倍宽。
    #[test]
    fn width_counts_columns_not_chars() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("中文"), 4, "汉字占两列");
        assert_eq!(display_width("a中"), 3);

        // 14 个汉字 = 28 列，刚好到上限；15 个就得截。
        let ok = "文".repeat(14);
        assert_eq!(ellipsize_middle(&ok, 28), ok);
        let too_long = "文".repeat(15);
        assert!(display_width(&ellipsize_middle(&too_long, 28)) <= 28);
    }

    /// 截断后的整条摘要要短到能塞进 Windows 的托盘提示。
    ///
    /// `NOTIFYICONDATA` 的提示文本有硬上限，超了就被系统直接切掉，用户
    /// 连百分比都看不到——实机上就是这个现象。
    #[test]
    fn summary_stays_short_enough_for_the_windows_tooltip() {
        let s = TrayStatus::new(1);
        s.note_transfer(TransferProgress {
            sending: false,
            name: "这是一个特别特别特别长的文件名用来测试截断是否生效.mkv".into(),
            done: 42,
            total: 100,
        });
        let sum = s.summary();
        assert!(
            sum.chars().count() < 100,
            "摘要 {} 字符，Windows 托盘提示放不下：{sum}",
            sum.chars().count()
        );
        assert!(sum.contains(".mkv"), "扩展名得看得见：{sum}");
        assert!(sum.contains("42%"), "百分比不能被挤掉：{sum}");
    }

    /// 人工核对截断效果。
    #[test]
    #[ignore = "只为肉眼看效果"]
    fn manual_show_truncation() {
        for name in [
            "report.pdf",
            "2026年度第三季度产品发布会现场录像完整版第三部分.mp4",
            "Ubuntu-24.04.1-desktop-amd64-live-server-installer.iso",
            "会议纪要.docx",
            "IMG_20260807_143052_HDR_Portrait_Enhanced_Final_v3.heic",
            "备份-王信的Mac mini-2026-08-07-完整系统镜像.dmg",
        ] {
            let s = TrayStatus::new(1);
            s.note_transfer(TransferProgress {
                sending: false,
                name: name.into(),
                done: 4_200_000_000,
                total: 10_000_000_000,
            });
            println!("原名 {name}");
            println!("  → {}", s.summary());
        }
    }
}
