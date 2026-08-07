//! 托盘展示的同步状态。
//!
//! 中枢更新、托盘读取，两边通过这个共享句柄通信。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
#[derive(Clone, Default)]
pub struct TrayStatus {
    data: Arc<Mutex<StatusData>>,
    paused: Arc<AtomicBool>,
    /// 同步中枢是否已停止工作（异常退出）。
    hub_dead: Arc<AtomicBool>,
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
        }
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

