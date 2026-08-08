//! 同步引擎：平台无关的决策核心。
//!
//! 引擎不做任何 I/O，只回答两个问题：
//!   1. 本地剪贴板变化了 —— 要不要广播出去？（`on_local_change`）
//!   2. 收到远端剪贴板消息 —— 要不要写入本地系统剪贴板？（`on_remote_message`）
//!
//! 两个方向共用一套"防回环"状态：当我们把远端内容写入本地系统剪贴板时，
//! 平台监听会再次触发 `on_local_change`；引擎通过登记"预期写入哈希"识别
//! 这类回声并抑制，从而不会把刚收到的内容又广播回去（形成风暴）。

use crate::content::ClipContent;
use crate::device::DeviceId;

/// **内联内容**（文本、图片）的硬上限。
///
/// 这两类是**推**过去的——图片的 RGBA 就躺在 `Clip` 消息里——所以没有"先看看
/// 多大再决定要不要"这回事：字节到岸了再判断，一个字节也省不下来。它们只能有
/// 一个防失控的硬上限，而不是一个需要用户权衡的旋钮。
///
/// 100 MiB 是"够用得离谱"的量：5000×5000 的截图连 RGBA 也才 100 MB，文本更不
/// 可能接近。真正需要用户拿主意的是**文件**，那是接收侧
/// `Settings::auto_fetch_bytes` 的事——文件只传元数据，拉不拉由收的人说了算。
pub const INLINE_MAX_BYTES: usize = 100 * 1024 * 1024;

/// 类型开关。
///
/// **这里没有大小上限**：文件的大小由接收方自己决定（见 `INLINE_MAX_BYTES`
/// 的说明），发送方一律照发元数据。原先这里有个 `max_bytes` 装在发送侧，
/// 结果是接收方的设置对它自己毫无保护——A 设「不限」就能把 5 GB 推给设了
/// 100 MiB 的 B，而带宽和磁盘全花在 B 身上。
#[derive(Debug, Clone)]
pub struct Limits {
    /// 是否同步图片。
    pub allow_image: bool,
    /// 是否同步文件。
    pub allow_files: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            allow_image: true,
            allow_files: true,
        }
    }
}

/// 本地变化被跳过的原因（用于日志/托盘提示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 这是我们自己刚写入的内容触发的回声。
    Echo,
    /// 与上次广播内容相同（去重）。
    Duplicate,
    /// 超过大小上限。
    TooLarge,
    /// 该类型被禁用。
    KindDisabled,
    /// 平台层判定为敏感/瞬态内容。
    Sensitive,
    /// 同步已暂停。
    Paused,
}

/// 对"本地剪贴板变化"的决策结果。
#[derive(Debug, Clone, PartialEq)]
pub enum LocalDecision {
    /// 应广播：携带序号与内容哈希。
    Broadcast { seq: u64, content_hash: u64 },
    /// 跳过，附原因。
    Skip(SkipReason),
}

/// 对"收到远端消息"的决策结果。
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteDecision {
    /// 应写入本地系统剪贴板。
    Apply,
    /// 跳过（重复/暂停）。
    Skip(SkipReason),
}

/// 同步引擎状态机。
///
/// 单线程使用；`clipsync-app` 通过 channel 串行化本地事件与远端事件后
/// 交给引擎，避免并发。
#[derive(Debug)]
pub struct SyncEngine {
    device_id: DeviceId,
    limits: Limits,
    paused: bool,

    /// 本设备下一个要用的广播序号。
    next_seq: u64,

    /// 最近一次"我方状态"（无论来自本地复制还是应用远端内容）的内容哈希。
    /// 用于去重：本地再次出现相同内容不重复广播。
    last_hash: Option<u64>,

    /// 预期写入哈希：调用方即将/已把此内容写入系统剪贴板，
    /// 由此产生的下一次本地变化应被当作回声抑制。
    ///
    /// 用集合而非单值，容忍平台监听的轻微乱序/延迟。
    ///
    /// **为什么要限容**：登记是"一写一销"的，但回声不保证兑现——写入剪贴板
    /// 后用户立刻复制了别的内容，去抖动会把这次变化合并掉，那条哈希就再也
    /// 没人来消费。引擎不碰时间（无法按时长过期），故改用容量上限 + FIFO
    /// 淘汰：常驻进程跑上几个月，集合也不会无限涨。
    pending_echo: std::collections::HashSet<u64>,
    /// 回声登记的先后顺序，用于超出容量时淘汰最旧的。
    pending_echo_order: std::collections::VecDeque<u64>,

    /// 曾被判定为敏感的内容哈希。
    ///
    /// **为什么需要记住**：敏感标记可能在内容仍留在剪贴板时消失——例如密码
    /// 管理器写入"密码 + 排除标记"后退出，系统 flush 剪贴板时会丢弃空数据的
    /// 自定义格式，文本却仍在。此时剪贴板序列号变化，我们重新读到的是"无标记
    /// 的密码"，若不记住就会把它广播出去。实测确认过这个泄漏路径。
    ///
    /// 因此一旦判定敏感，就按内容记住；即便标记后来消失也继续跳过。
    sensitive_hashes: std::collections::HashSet<u64>,
    /// 敏感哈希的加入顺序，用于按容量淘汰最旧的，避免无限增长。
    sensitive_order: std::collections::VecDeque<u64>,
}

/// 记住多少条敏感内容哈希。
///
/// 足够覆盖"标记消失后重新出现"的窗口，又不会无限增长；超出后淘汰最旧的。
const SENSITIVE_MEMORY: usize = 64;

/// 最多同时挂着多少条待兑现的回声登记。
///
/// 正常情况下集合里至多一两条（写入剪贴板后下一轮监听就消费掉了）。取 16
/// 足以覆盖平台监听的乱序与延迟，又能保证未兑现的登记不会堆积。
const PENDING_ECHO_CAPACITY: usize = 16;

impl SyncEngine {
    pub fn new(device_id: DeviceId, limits: Limits) -> Self {
        Self {
            device_id,
            limits,
            paused: false,
            next_seq: 0,
            last_hash: None,
            pending_echo: std::collections::HashSet::new(),
            pending_echo_order: std::collections::VecDeque::new(),
            sensitive_hashes: std::collections::HashSet::new(),
            sensitive_order: std::collections::VecDeque::new(),
        }
    }

    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// 替换大小上限与类型开关（用户在界面上改设置时调用）。
    ///
    /// 只影响此后的判定，不追溯已广播的内容。类型开关**收发两侧都生效**，
    /// 所以界面上就叫"同步图片"——关掉即彻底不同步该类型。
    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// 该类型是否被用户关掉了。收发两侧共用同一判断。
    fn kind_disabled(&self, content: &ClipContent) -> bool {
        match content.kind() {
            crate::content::ContentKind::Image => !self.limits.allow_image,
            crate::content::ContentKind::Files => !self.limits.allow_files,
            crate::content::ContentKind::Text => false,
        }
    }

    /// 当前生效的上限与开关。
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// 处理本地剪贴板变化。
    ///
    /// `sensitive` 由平台层判定（macOS ConcealedType / Windows 排除标记等）。
    /// 返回是否应广播；若广播，调用方用返回的 seq/hash 构造 `SyncMessage::Clip`。
    pub fn on_local_change(&mut self, content: &ClipContent, sensitive: bool) -> LocalDecision {
        if self.paused {
            return LocalDecision::Skip(SkipReason::Paused);
        }

        let hash = content.content_hash();

        // 1) 回声抑制：这是我们刚写入系统剪贴板的内容。
        if self.take_echo(hash) {
            // 同步 last_hash，使后续同内容的真实本地复制也能正确去重。
            self.last_hash = Some(hash);
            return LocalDecision::Skip(SkipReason::Echo);
        }

        // 2) 敏感内容不外传。
        //
        // 除了平台层当下的判定，还要查"曾经判定过敏感"的记录——标记可能在
        // 内容仍在剪贴板时消失（如密码管理器退出触发系统 flush），此时若只看
        // 当下标记就会把密码泄漏出去。
        if sensitive || self.sensitive_hashes.contains(&hash) {
            self.remember_sensitive(hash);
            return LocalDecision::Skip(SkipReason::Sensitive);
        }

        // 3) 类型开关。
        if self.kind_disabled(content) {
            return LocalDecision::Skip(SkipReason::KindDisabled);
        }

        // 4) 内联内容的硬上限。
        //
        // 文件**不受此限**：它们只把元数据发出去，实际字节由接收方按自己的
        // 「自动取回上限」决定拉不拉（见 `hub_incoming::begin_incoming_files`）。
        // 复制多大的文件都照常通告，是这套语义的前提——发送方无从知道对方
        // 的网络与磁盘状况，那个判断本来就该由收的人来做。
        if !matches!(content, ClipContent::Files(_)) && content.byte_size() > INLINE_MAX_BYTES {
            return LocalDecision::Skip(SkipReason::TooLarge);
        }

        // 5) 去重：与当前状态相同则不重复广播。
        if self.last_hash == Some(hash) {
            return LocalDecision::Skip(SkipReason::Duplicate);
        }

        // 通过：更新状态并分配序号。
        self.last_hash = Some(hash);
        let seq = self.next_seq;
        self.next_seq += 1;
        LocalDecision::Broadcast {
            seq,
            content_hash: hash,
        }
    }

    /// 处理来自远端的剪贴板消息。
    ///
    /// 返回是否应写入本地系统剪贴板。若应写入，调用方必须在**实际写入前**
    /// 调用 [`Self::expect_echo`] 登记该内容哈希，以抑制随之而来的本地回声。
    pub fn on_remote_clip(&mut self, content: &ClipContent, content_hash: u64) -> RemoteDecision {
        if self.paused {
            return RemoteDecision::Skip(SkipReason::Paused);
        }

        // 类型开关同样拦接收。早先只拦发送，于是菜单不得不写成"发送图片到
        // 其它设备"——否则用户关掉后仍然收到图片，会觉得开关是坏的。
        // 收发一致后，"同步图片"这个名字才名副其实。
        if self.kind_disabled(content) {
            return RemoteDecision::Skip(SkipReason::KindDisabled);
        }

        // 已经是当前内容 —— 无需重复写入（例如两端几乎同时复制了相同内容）。
        if self.last_hash == Some(content_hash) {
            return RemoteDecision::Skip(SkipReason::Duplicate);
        }

        // 记录为当前状态；防回环由 expect_echo 负责。
        self.last_hash = Some(content_hash);
        let _ = content; // 内容本身由调用方写入系统剪贴板
        RemoteDecision::Apply
    }

    /// 登记"即将写入系统剪贴板"的内容哈希，使随之触发的本地变化被识别为回声。
    ///
    /// 必须在真正写入系统剪贴板之前调用。
    ///
    /// 超出 [`PENDING_ECHO_CAPACITY`] 时淘汰最早登记的一条——它多半是一次
    /// 永远不会兑现的回声（写入后剪贴板又被别的内容盖掉了）。
    pub fn expect_echo(&mut self, content_hash: u64) {
        if self.pending_echo.insert(content_hash) {
            self.pending_echo_order.push_back(content_hash);
            while self.pending_echo_order.len() > PENDING_ECHO_CAPACITY {
                if let Some(old) = self.pending_echo_order.pop_front() {
                    self.pending_echo.remove(&old);
                }
            }
        }
    }

    /// 消费一条回声登记，同时把它从顺序表里摘掉。
    fn take_echo(&mut self, hash: u64) -> bool {
        if self.pending_echo.remove(&hash) {
            if let Some(pos) = self.pending_echo_order.iter().position(|h| *h == hash) {
                self.pending_echo_order.remove(pos);
            }
            true
        } else {
            false
        }
    }

    /// 忘记"当前内容"记录。
    ///
    /// 用于远端内容未能真正落地的情形——典型是文件内容传输中断或校验失败。
    /// 此时若仍记着该哈希，对端重新发送相同内容会被误判为重复而跳过，导致
    /// 用户永远同步不过来。清除后即可正常重试。
    pub fn forget_current(&mut self) {
        self.last_hash = None;
    }

    /// 记住一个敏感内容哈希，按容量淘汰最旧的。
    fn remember_sensitive(&mut self, hash: u64) {
        if self.sensitive_hashes.insert(hash) {
            self.sensitive_order.push_back(hash);
            while self.sensitive_order.len() > SENSITIVE_MEMORY {
                if let Some(old) = self.sensitive_order.pop_front() {
                    self.sensitive_hashes.remove(&old);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
