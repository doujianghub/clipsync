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
        }
    }

    /// 记一次文件分块的收发。收发两侧都调用，图标据此脉冲。
    ///
    /// 每个分块调一次，所以必须极廉价：一次 `Instant::elapsed` 加一次原子写。
    pub fn note_transfer(&self) {
        let ms = self.started.elapsed().as_millis() as u64;
        // 用 max 语义没必要——晚到的写覆盖早到的也只是让脉冲多持续几毫秒。
        self.last_transfer.store(ms.max(1), Ordering::Relaxed);
    }

    /// 眼下是否正在传文件（据此让托盘图标脉冲）。
    pub fn is_transferring(&self) -> bool {
        let last = self.last_transfer.load(Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        let now = self.started.elapsed().as_millis() as u64;
        now.saturating_sub(last) < TRANSFER_LINGER.as_millis() as u64
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
