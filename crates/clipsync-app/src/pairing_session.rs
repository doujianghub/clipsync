//! 主持配对的会话槽位。
//!
//! 一个"正在等待对方连入"的会话有三件事要对外可见：**当前配对码**（重复点
//! 菜单时要把同一个码再显示一遍）、**还剩多久**（托盘菜单里的实时倒计时）、
//! 以及**怎么把它掐掉**（「换个配对码」）。这三件都被读写于多个线程——托盘
//! 循环、主持线程、弹窗线程——所以单独成一个上了锁的小状态机，而不是散在
//! 交互流程里。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 一个正在等待对方连入的主持会话。
struct Session {
    /// 配对码。占位到 `host()` 生成出来之前是空串。
    code: String,
    /// 到期时刻。拿到码之前还不知道。
    deadline: Option<Instant>,
    /// 置起即请求结束本轮（用户点了「换个配对码」）。
    cancel: Arc<AtomicBool>,
}

/// 当前有效的配对码及其剩余时间，供托盘菜单与弹窗使用。
pub(crate) struct LiveCode {
    pub(crate) code: String,
    pub(crate) remaining: Duration,
}

/// 保证同一时刻只有一个"主持配对"会话在跑，并把会话状态开放给托盘。
///
/// 没有这层控制时，用户第二次点「显示配对码」会新起一个线程去 bind 已被占用
/// 的 47685，直接抛出 `os error 10048`（Windows）/ `48`（macOS）给用户看。
/// 而用户的真实意图通常只是**再看一眼那个码**——所以这里不报错、也不新开
/// 会话，而是把当前会话的配对码重新弹出来。
#[derive(Clone, Default)]
pub(crate) struct PairingHostSlot {
    active: Arc<Mutex<Option<Session>>>,
}

impl PairingHostSlot {
    /// 尝试占用槽位；已被占用时返回 `None`（当前会话见 [`live`](Self::live)）。
    pub(crate) fn try_acquire(&self) -> Option<PairingHostGuard> {
        let mut g = self.active.lock().unwrap();
        if g.is_some() {
            return None;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *g = Some(Session {
            code: String::new(), // 先占位，拿到码后再补
            deadline: None,
            cancel: cancel.clone(),
        });
        Some(PairingHostGuard {
            slot: self.clone(),
            cancel,
        })
    }

    /// 记下本轮的配对码与到期时刻。
    pub(crate) fn begin(&self, code: &str, deadline: Instant) {
        if let Some(s) = self.active.lock().unwrap().as_mut() {
            s.code = code.to_string();
            s.deadline = Some(deadline);
        }
    }

    /// 当前有效的配对码；无会话、码还没生成、或已过期时为 `None`。
    ///
    /// **过期也算没有**：托盘据此决定菜单上写「显示配对码…」还是倒计时，
    /// 而 `host()` 察觉超时要等到下一个轮询周期。宁可菜单早半拍恢复原样，
    /// 也别显示「剩 0 秒」或负数。
    pub(crate) fn live(&self) -> Option<LiveCode> {
        let g = self.active.lock().unwrap();
        let s = g.as_ref()?;
        if s.code.is_empty() {
            return None;
        }
        let remaining = s.deadline?.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        Some(LiveCode {
            code: s.code.clone(),
            remaining,
        })
    }

    /// 请求换一个配对码，结束当前这一轮。返回是否确实有会话可结束。
    ///
    /// `only_if` 给定时，只有当前会话的码与之相同才动手——弹窗可能是上一轮
    /// 留在屏幕上的旧窗口，那时按下按钮不该把**这一轮**掐掉。
    pub(crate) fn request_new_code(&self, only_if: Option<&str>) -> bool {
        let g = self.active.lock().unwrap();
        let Some(s) = g.as_ref() else { return false };
        if s.code.is_empty() || only_if.is_some_and(|c| c != s.code) {
            return false;
        }
        s.cancel.store(true, Ordering::Release);
        true
    }

    fn release(&self) {
        *self.active.lock().unwrap() = None;
    }
}

/// 持有期间槽位被占用；**丢弃即释放**，包括 host() 提前返回错误的路径。
pub(crate) struct PairingHostGuard {
    slot: PairingHostSlot,
    /// 本轮的取消标志，交给 `host()` 轮询。
    pub(crate) cancel: Arc<AtomicBool>,
}

impl Drop for PairingHostGuard {
    fn drop(&mut self) {
        self.slot.release();
    }
}

#[cfg(test)]
#[path = "pairing_session_tests.rs"]
mod tests;
