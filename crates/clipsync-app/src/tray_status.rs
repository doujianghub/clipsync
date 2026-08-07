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

/// 界面上文件名保留的头尾字符数：`前5…后5`。
///
/// 名字长到看不完时，真正有辨识度的就是开头和结尾——结尾还带着扩展名与
/// 序号。中间那一大段（日期、参数、哈希）反而是最不需要看清的部分。
/// 11 个字符（5+1+5）足够认出是哪个文件，也让托盘那行不至于抖。
const NAME_HEAD: usize = 5;
const NAME_TAIL: usize = 5;

/// Windows 托盘提示的硬上限：`NOTIFYICONDATA.szTip` 是 64 个 UTF-16 单元
/// （63 个字符 + 结尾的 0），超出部分被系统直接切掉，不换行也不省略。
///
/// 实机截图里正好断在第 64 个字符上，后半截连百分比都看不到。注意这个限制
/// 数的是**字符数**（汉字也只算一个 UTF-16 单元），与菜单那行能占多宽是两回
/// 事——这也是提示与菜单项必须分开生成的原因。
const TOOLTIP_MAX_CHARS: usize = 63;

/// 把字符串硬塞进 `max` 个字符，超了就尾部省略。
///
/// 最后一道保险。正常路径上文案已经短于上限，走到这里说明哪里算漏了——
/// 宁可自己带个省略号，也别让系统在半截字上切一刀。
fn fit_chars(s: &str, max: usize) -> String {
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
fn ellipsize_middle(s: &str) -> String {
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
fn bytes_pair(done: u64, total: u64) -> String {
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
fn describe(st: &ProgressState, shorten: bool) -> String {
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

impl TrayStatus {
    /// 悬停提示。与菜单里那行**不是**同一份文案。
    ///
    /// 传输中固定排成**两行**，行内容各司其职：
    ///
    /// ```text
    /// 接收 2026年…分.mp4 42%
    /// 3.9 / 9.3 GiB · 47.3 MiB/s
    /// ```
    ///
    /// **为什么自己换行**：让系统按宽度自动折行的话，速率位数一变
    /// （`9.8 MiB/s` ↔ `123.4 MiB/s`）总长就在折行临界点上下浮动，提示一会儿
    /// 一行、一会儿两行地跳。自己定死行数，宽度再变也只是行内长短的事。
    ///
    /// **为什么去掉 `ClipSync — ` 前缀**：63 个字符的额度太紧，两行加起来最坏
    /// 要 55 个，前缀那 11 个字符会顶出去。而鼠标正悬在 ClipSync 的图标上，
    /// 本来也不必自报家门。空闲时字数宽裕，前缀就留着。
    pub fn tooltip(&self) -> String {
        let text = match self.progress_tooltip() {
            Some(p) => p,
            None => self.summary(),
        };
        fit_chars(&text, TOOLTIP_MAX_CHARS)
    }

    fn progress_tooltip(&self) -> Option<String> {
        if !self.is_transferring() {
            return None;
        }
        let g = self.progress.lock().unwrap();
        let st = g.as_ref()?;

        let dir = if st.p.sending { "发送" } else { "接收" };
        let pct = if st.p.total > 0 {
            st.p.done.saturating_mul(100) / st.p.total
        } else {
            0
        };
        let name = ellipsize_middle(&st.p.name);

        let mut second = bytes_pair(st.p.done, st.p.total);
        let moved = st.p.done.saturating_sub(st.since_bytes);
        let secs = st.since.elapsed().as_secs_f64();
        if moved > 0 && secs >= 0.5 {
            let rate = (moved as f64 / secs) as u64;
            second.push_str(&format!(" · {}/s", crate::tray::human_bytes(rate)));
        }
        Some(format!("{dir} {name} {pct}%\n{second}"))
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

    /// 单位相同就只写一次——重复的单位不带信息，白占五列。
    #[test]
    fn byte_pairs_drop_the_repeated_unit() {
        assert_eq!(bytes_pair(4 << 30, 9 << 30), "4 / 9 GiB");
        // 单位不同就得都写全，否则读者会误以为同一量级。
        let mixed = bytes_pair(151 << 20, 17 << 30);
        assert!(mixed.contains("MiB") && mixed.contains("GiB"), "{mixed}");
    }

    /// 短名字原样保留，不该无端加省略号。
    #[test]
    fn short_names_pass_through() {
        assert_eq!(ellipsize_middle("a.txt"), "a.txt");
        assert_eq!(ellipsize_middle("报告.pdf"), "报告.pdf");
        // 正好 5+1+5 也不截。
        assert_eq!(ellipsize_middle("abcdeXfghij"), "abcdeXfghij");
    }

    /// 超长时固定取头 5 尾 5，**扩展名必须留着**。
    #[test]
    fn long_names_keep_five_at_each_end() {
        let out = ellipsize_middle("2026年度第三季度产品发布会现场录像完整版第三部分.mp4");
        assert_eq!(out, "2026年…分.mp4");
        assert_eq!(out.chars().count(), 11);

        let iso = ellipsize_middle("Ubuntu-24.04.1-desktop-amd64-live-server-installer.iso");
        assert_eq!(iso, "Ubunt…r.iso");
    }

    /// 截断长度**不随剩余空间浮动**。
    ///
    /// 回归自实机观感："有时候换行有时候不换行"。若按剩余预算分配头尾，
    /// 速率位数一变（9.8 ↔ 123.4 MiB/s）名字长度就跟着变，总宽在折行临界点
    /// 上下抖，提示一会儿一行一会儿两行。
    #[test]
    fn name_length_is_fixed_regardless_of_context() {
        let long = "IMG_20260807_143052_HDR_Portrait_Enhanced_Final_v3.heic";
        let a = ellipsize_middle(long);
        let b = ellipsize_middle(long);
        assert_eq!(a, b);
        assert_eq!(a.chars().count(), NAME_HEAD + 1 + NAME_TAIL);
    }

    /// 托盘提示**任何情况下**都不能超过 63 个字符。
    ///
    /// 回归自实机截图：文案在第 64 个字符上被系统一刀切断，后半截连百分比
    /// 都看不到。上一轮只截了文件名，可固定部分（`ClipSync — 接收 ` 加上
    /// 绝对字节数与速率）本身就占掉五十来个，光截名字不够。
    #[test]
    fn tooltip_never_exceeds_the_windows_limit() {
        let cases: [(&str, u64, u64); 4] = [
            ("sha256-1194192cf2b8e4a09d7c3f5061e2a78863006a.tar.zst", 159_000_000, 18_500_000_000),
            ("这是一个特别特别特别特别长的中文文件名用来测试截断.mkv", 1, 100),
            ("a.txt", 50, 100),
            (&"x".repeat(300), 1, 2),
        ];
        for (name, done, total) in cases {
            let s = TrayStatus::new(3);
            s.set_connected_ids(["a".into(), "b".into()].into_iter().collect());
            s.note_transfer(TransferProgress {
                sending: false,
                name: name.into(),
                done,
                total,
            });
            // 走一遍会显示速率的分支：预算是倒推出来的，速率位数变化不该把
            // 总长顶出去。
            std::thread::sleep(std::time::Duration::from_millis(600));
            s.note_transfer(TransferProgress {
                sending: false,
                name: name.into(),
                done: done + 12_345_678,
                total,
            });

            let tip = s.tooltip();
            let n = tip.chars().count();
            assert!(n <= 63, "提示 {n} 字符，超了：{tip}");
            assert!(tip.contains('%'), "百分比不能被挤掉：{tip}");

            // 固定两行：自己换行才不会随宽度抖。
            let lines: Vec<&str> = tip.split('\n').collect();
            assert_eq!(lines.len(), 2, "传输中的提示应恒为两行：{tip:?}");
            assert!(lines[0].contains('%'), "第一行给方向、名字、进度：{tip:?}");
            assert!(lines[1].contains('/'), "第二行给已传/总量：{tip:?}");
            // 任一行都不该长到被系统再折一次。
            for l in &lines {
                assert!(l.chars().count() <= 40, "行太长会被再折一次：{l}");
            }
        }
    }

    /// 没有传输时提示也得守住上限（设备名可以很长）。
    #[test]
    fn idle_tooltip_also_fits() {
        let s = TrayStatus::new(9);
        assert!(s.tooltip().chars().count() <= 63);
        s.set_connected_ids((0..9).map(|i| i.to_string()).collect());
        assert!(s.tooltip().chars().count() <= 63);
    }

    /// 菜单与提示都要给出绝对字节数——只看百分比不知道还剩多少。
    #[test]
    fn menu_summary_keeps_the_absolute_bytes() {
        let s = TrayStatus::new(1);
        s.note_transfer(TransferProgress {
            sending: false,
            name: "video.mkv".into(),
            done: 0,
            total: 17_300_000_000,
        });
        std::thread::sleep(std::time::Duration::from_millis(600));
        s.note_transfer(TransferProgress {
            sending: false,
            name: "video.mkv".into(),
            done: 151_800_000,
            total: 17_300_000_000,
        });
        let sum = s.summary();
        assert!(sum.contains("GiB") || sum.contains("MiB"), "菜单里该有字节数：{sum}");
        // 提示里也要有已传/总量——只看百分比不知道还剩多少。
        let tip = s.tooltip();
        assert!(tip.contains(" / "), "提示第二行该给出已传/总量：{tip}");
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
            // 走到会显示速率的分支——那才是传输中的常态，文案也最长。
            std::thread::sleep(std::time::Duration::from_millis(600));
            s.note_transfer(TransferProgress {
                sending: false,
                name: name.into(),
                done: 4_230_000_000,
                total: 10_000_000_000,
            });
            println!("原名 {name}");
            println!("  菜单 {}", s.summary());
            let tip = s.tooltip();
            for (i, l) in tip.split('\n').enumerate() {
                println!("  提示{} {l}", i + 1);
            }
            println!("       [共 {} 字符]", tip.chars().count());
        }
    }
}
