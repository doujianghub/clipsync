//! 已配对设备表。
//!
//! 独立成模块是因为它是**运行期可变**的共享状态：拨号线程、入站认证、
//! 配对流程、托盘设备列表都要读写它。放在 `net_manager` 里会让那个文件
//! 同时承担"连接管理"与"设备目录"两件事。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use clipsync_core::DeviceId;
use clipsync_net::pairing::PairingRecord;

/// 已知对端的最小信息（来自配对记录）。
#[derive(Clone)]
pub struct KnownPeer {
    pub device: DeviceId,
    pub name: String,
    pub static_public_key: Vec<u8>,
}

impl From<PairingRecord> for KnownPeer {
    fn from(r: PairingRecord) -> Self {
        Self {
            device: r.device,
            name: r.name,
            static_public_key: r.static_public_key,
        }
    }
}

/// 已配对设备表，可在运行期增补。
///
/// **为什么不是启动时定格的 `Arc<Vec<_>>`**：配对现在可以从托盘发起
/// （「显示配对码…」/「输入配对码…」），配对成功时进程正在运行。若这张表
/// 是启动快照，新配对的设备要**重启程序**才会被拨号线程看见、才会通过入站
/// 认证——用户点完菜单、看到"配对成功"，然后发现什么也同步不了，只能靠
/// 猜出"得重启一下"。
///
/// 读多写极少（几秒一次读、一辈子几次写），用 `Mutex` + 读时克隆即可，
/// 不值得引入 `RwLock` 的复杂度。
#[derive(Clone, Default)]
pub struct KnownPeers {
    inner: Arc<Mutex<Vec<KnownPeer>>>,
    /// 变更计数。
    ///
    /// 各连接据此知道"设备表变了，该把新成员引荐出去了"。没有它就只能靠
    /// 定时轮询：新配了一台设备，已建立的连接要等下一个周期才把它介绍出去
    /// ——用户刚配完却发现另外两台互相不认识，只能干等。
    ///
    /// 每轮比对一个整数即可，不必拿锁拷贝整张表。
    version: Arc<std::sync::atomic::AtomicU64>,
    /// 变更通知：拨号线程等在这上面，配对/引荐一登记就立刻醒。
    changed: Arc<(Mutex<()>, std::sync::Condvar)>,
    /// 设备台数的无锁镜像，随本表一起变。
    ///
    /// 托盘要显示「已连接 m / n 台」，那个 n 就是这张表的长度。原先托盘自己
    /// 存了一份、由配对流程手工同步——而写这张表的地方不止配对流程：引荐来的
    /// 设备、对端广播的移出，都在 `net_pump` 里直接改表。于是实机上出现过
    /// 「已连接 2 / 1 台」：分子来自连接，分母停在一次也没更新过的旧值。
    ///
    /// 把台数挂在表自己身上，两者就不可能再分家；代价只是每次增删多一次
    /// 原子写。
    count: Arc<AtomicUsize>,
}

impl KnownPeers {
    pub fn new(peers: Vec<KnownPeer>) -> Self {
        Self {
            count: Arc::new(AtomicUsize::new(peers.len())),
            inner: Arc::new(Mutex::new(peers)),
            version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            changed: Arc::new((Mutex::new(()), std::sync::Condvar::new())),
        }
    }

    /// 台数的只读句柄，供托盘直接读——它拿到的永远是这张表的真实长度。
    pub fn counter(&self) -> Arc<AtomicUsize> {
        self.count.clone()
    }

    /// 变更计数。值变了就说明设备表被动过。
    pub fn version(&self) -> u64 {
        self.version.load(std::sync::atomic::Ordering::Acquire)
    }

    fn bump(&self) {
        self.version
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        // 先取锁再通知：等待方是在持锁状态下读的版本号，这样不会丢唤醒。
        let _g = self.changed.0.lock().unwrap();
        self.changed.1.notify_all();
    }

    /// 等设备表发生变化，最多等 `timeout`。
    ///
    /// **为什么需要**：拨号线程原先是死等一个退避间隔，而退避连不上时会翻倍
    /// 到上限。于是刚配好一台设备、或刚经引荐认识一台，都得干等下一轮——
    /// 实机日志里"经 KPC 认识了 MacBook Pro"到真正连上隔了 **45 秒**。
    /// 用户的感受是"配对完还得等半天，引荐更慢"。
    ///
    /// 换成条件变量之后，登记与拨号之间几乎没有延迟，且不靠轮询——不该为了
    /// 反应快就让一个后台线程每 200 毫秒醒一次。
    pub fn wait_for_change(&self, since: u64, timeout: std::time::Duration) {
        let g = self.changed.0.lock().unwrap();
        // 持锁期间再确认一次：`bump` 必须拿到同一把锁才能通知，所以这之后
        // 发生的变更一定能唤醒我们，不存在丢唤醒的窗口。
        if self.version() != since {
            return;
        }
        let _ = self.changed.1.wait_timeout(g, timeout);
    }

    /// 当前全部已配对设备。
    pub fn snapshot(&self) -> Vec<KnownPeer> {
        self.inner.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    pub fn contains(&self, device: &DeviceId) -> bool {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .any(|p| &p.device == device)
    }

    /// 按静态公钥查找——入站连接的认证依据。
    pub fn find_by_static_key(&self, key: &[u8]) -> Option<KnownPeer> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.static_public_key == key)
            .cloned()
    }

    /// 加入或更新一台设备（按 device id 去重）。
    pub fn upsert(&self, peer: KnownPeer) {
        {
            let mut g = self.inner.lock().unwrap();
            match g.iter_mut().find(|p| p.device == peer.device) {
                Some(existing) => *existing = peer,
                None => g.push(peer),
            }
            // 在锁内发布：持锁者看到的表长与台数永远一致。
            self.count.store(g.len(), Ordering::Release);
        }
        self.bump();
    }

    /// 移除一台设备。返回是否确实移除了。
    ///
    /// 移除后拨号线程不再拨它，入站握手也查不到其公钥而拒绝连接——解除配对
    /// 因此立即生效，不必重启。
    pub fn remove(&self, device: &DeviceId) -> bool {
        let changed = {
            let mut g = self.inner.lock().unwrap();
            let before = g.len();
            g.retain(|p| &p.device != device);
            self.count.store(g.len(), Ordering::Release);
            g.len() != before
        };
        if changed {
            self.bump();
        }
        changed
    }
}

#[cfg(test)]
#[path = "known_peers_tests.rs"]
mod tests;
