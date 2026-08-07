//! 已配对设备表。
//!
//! 独立成模块是因为它是**运行期可变**的共享状态：拨号线程、入站认证、
//! 配对流程、托盘设备列表都要读写它。放在 `net_manager` 里会让那个文件
//! 同时承担"连接管理"与"设备目录"两件事。

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
}

impl KnownPeers {
    pub fn new(peers: Vec<KnownPeer>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(peers)),
            version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// 变更计数。值变了就说明设备表被动过。
    pub fn version(&self) -> u64 {
        self.version.load(std::sync::atomic::Ordering::Acquire)
    }

    fn bump(&self) {
        self.version
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// 当前全部已配对设备。
    pub fn snapshot(&self) -> Vec<KnownPeer> {
        self.inner.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn contains(&self, device: &DeviceId) -> bool {
        self.inner.lock().unwrap().iter().any(|p| &p.device == device)
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
