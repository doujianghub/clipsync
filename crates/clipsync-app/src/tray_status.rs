//! 托盘展示的同步状态。
//!
//! 中枢更新、托盘读取，两边通过这个共享句柄通信。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[path = "tray_text.rs"]
mod text;

pub(crate) use text::ellipsize_middle;
use text::{bytes_pair, describe, fit_chars, TOOLTIP_MAX_CHARS};
#[cfg(test)]
use text::{NAME_HEAD, NAME_TAIL};

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

/// 一批挂起待取的文件的摘要，供托盘展示。
///
/// 只带"画界面要用的东西"：真正的文件清单在中枢手里，托盘不需要，也不该
/// 拿着一份会过期的副本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingFetchInfo {
    /// 来源设备 ID。托盘据此判断"对方在不在线"——不在线时点了取回只会石沉
    /// 大海，不如当场说清楚。
    pub from: String,
    pub first_name: String,
    pub count: usize,
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

/// 同步状态的一份快照，供托盘展示。
///
/// 只读快照，没有"两个字段要记得一起改"的问题：`connected` 由在线集合的长度
/// 导出，`paired` 现读设备表。要具体是哪几台在线，用
/// [`connected_devices`](TrayStatus::connected_devices)。
#[derive(Debug, Clone, Default)]
pub struct StatusData {
    /// 当前已连接的对端数。
    pub connected: usize,
    /// 已配对设备总数。
    pub paired: usize,
}

/// 线程安全的状态句柄：中枢更新，托盘读取。
#[derive(Clone)]
pub struct TrayStatus {
    /// 当前在线的设备 ID 集合。台数由它的长度导出，不另存。
    connected: Arc<Mutex<std::collections::HashSet<String>>>,
    /// 已配对台数。**不是自己存的一份**，而是 `KnownPeers` 那张表的台数句柄。
    ///
    /// 原先托盘自己存着这个数，由配对流程手工同步。可写那张表的地方有四处
    /// （亲手配对、引荐认识、本机移出、对端广播移出），只有前两处记得回头通知
    /// 托盘——实机上因此出现「已连接 2 / 1 台」：引荐来的设备连上了，分母却
    /// 停在旧值。一个数有两个副本就迟早对不上，所以干脆只留一份。
    paired: Arc<AtomicUsize>,
    paused: Arc<AtomicBool>,
    /// 同步中枢是否已停止工作（异常退出）。
    hub_dead: Arc<AtomicBool>,
    /// 进程启动时刻，配合 `last_transfer` 换算"多久之前"。
    started: Arc<Instant>,
    /// 进行中的传输及其速度锚点。
    progress: Arc<Mutex<Option<ProgressState>>>,
    /// 超过自动取回上限、等着用户点一下的那一批文件。
    ///
    /// 菜单项、图标角标、悬停提示三处都读它——一份来源，三处不会说不一样的话。
    pending: Arc<Mutex<Option<PendingFetchInfo>>>,
    /// 最后一次文件分块收发距 `started` 的毫秒数；0 表示从未传输过。
    ///
    /// **用"最后活动时刻"而不是"进行中计数"**：计数要在收发两侧各处出口
    /// 精确配对增减，漏掉任何一条错误路径就会永久泄漏，表现是图标一直闪个
    /// 不停——一个只在出错后才显现、且很难查的毛病。时间戳没有这个问题，
    /// 它自己会过期。
    last_transfer: Arc<AtomicU64>,
}

impl TrayStatus {
    /// 台数由外部（`KnownPeers`）持有的正路。
    pub fn tracking(paired: Arc<AtomicUsize>) -> Self {
        Self {
            connected: Arc::new(Mutex::new(std::collections::HashSet::new())),
            paired,
            paused: Arc::new(AtomicBool::new(false)),
            hub_dead: Arc::new(AtomicBool::new(false)),
            started: Arc::new(Instant::now()),
            last_transfer: Arc::new(AtomicU64::new(0)),
            progress: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(None)),
        }
    }

    /// 台数固定的独立实例，用于测试与无托盘模式。
    pub fn new(paired: usize) -> Self {
        Self {
            connected: Arc::new(Mutex::new(std::collections::HashSet::new())),
            paired: Arc::new(AtomicUsize::new(paired)),
            paused: Arc::new(AtomicBool::new(false)),
            hub_dead: Arc::new(AtomicBool::new(false)),
            started: Arc::new(Instant::now()),
            last_transfer: Arc::new(AtomicU64::new(0)),
            progress: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(None)),
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

    /// 中枢更新待取项；`None` 表示没有。
    pub fn set_pending(&self, p: Option<PendingFetchInfo>) {
        *self.pending.lock().unwrap() = p;
    }

    /// 当前待取项。
    pub fn pending(&self) -> Option<PendingFetchInfo> {
        self.pending.lock().unwrap().clone()
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

impl TrayStatus {

    /// 眼下是否正在传文件（据此让托盘图标脉冲、摘要报进度）。
    pub fn is_transferring(&self) -> bool {
        transfer_is_recent(
            self.started.elapsed().as_millis() as u64,
            self.last_transfer.load(Ordering::Relaxed),
        )
    }

    /// 更新在线设备集合。
    pub fn set_connected_ids(&self, ids: std::collections::HashSet<String>) {
        *self.connected.lock().unwrap() = ids;
    }

    /// 当前在线的设备 ID。
    pub fn connected_devices(&self) -> std::collections::HashSet<String> {
        self.connected.lock().unwrap().clone()
    }

    pub fn snapshot(&self) -> StatusData {
        StatusData {
            connected: self.connected.lock().unwrap().len(),
            paired: self.paired.load(Ordering::Acquire),
        }
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
    /// **待取项在空闲时才提**：传输中那两行已经把 63 个字符占满了，而且
    /// 眼下正在动的那件事更值得看。等它传完，提示自然会退回空闲态并带上这一行。
    pub fn tooltip(&self) -> String {
        let text = match self.progress_tooltip() {
            Some(p) => p,
            None => {
                let mut s = self.summary();
                if let Some(p) = self.pending() {
                    s.push_str(&format!(
                        "\n{} 项待取回（{}）",
                        p.count,
                        crate::tray::human_bytes(p.total)
                    ));
                }
                s
            }
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
#[path = "tray_status_tests.rs"]
mod tests;
