//! 地址簿：汇聚各来源的对端候选地址，供拨号按优先级尝试。
//!
//! 三个**通用**地址来源（都不依赖任何特定组网产品）：
//!   1. **配对时交换** —— 配对完成即知对端全部地址，写入配对记录持久化。
//!   2. **局域网信标** —— 同网段设备周期性组播宣告，动态发现（IP 变化也能跟上）。
//!   3. **对端通告** —— 已连接的对端通过加密通道持续同步其地址表；只要还有
//!      任意一条路径连通，其它路径的地址就能被学到。
//!
//! 地址的优先级由本机网络位置决定（见 [`clipsync_net::peer::classify`]）：
//! 同网段直连 > 覆盖网/VPN > 公网。因此同一个地址在不同设备上可能有不同优先级，
//! 这正是期望行为。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use clipsync_core::DeviceId;
use clipsync_net::local::local_networks;
use clipsync_net::peer::{classify, AddrSource, Candidate, LocalNetworks, PeerAddresses};

/// 线程安全的地址簿。可克隆，共享同一份数据。
#[derive(Clone)]
pub struct AddrBook {
    inner: Arc<Mutex<HashMap<DeviceId, PeerAddresses>>>,
    /// 本机网段快照，用于地址分类。
    local: Arc<Mutex<LocalNetworks>>,
}

impl AddrBook {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            local: Arc::new(Mutex::new(local_networks())),
        }
    }

    /// 重新采样本机网段（网络切换后调用，使分类保持准确）。
    pub fn refresh_local_networks(&self) {
        *self.local.lock().unwrap() = local_networks();
    }

    /// 加入一批对端地址，按本机网络位置自动分类。
    ///
    /// 无法分类的地址（回环之外的链路本地、组播等）会被忽略。
    pub fn add_addrs(
        &self,
        device: &DeviceId,
        addrs: impl IntoIterator<Item = SocketAddr>,
        source: AddrSource,
    ) {
        let local = self.local.lock().unwrap().clone();
        let mut book = self.inner.lock().unwrap();
        let entry = book.entry(device.clone()).or_default();
        for sa in addrs {
            if let Some(class) = classify(sa.ip(), &local) {
                entry.upsert(Candidate::new(sa, class, source));
            }
        }
    }

    /// 按优先级返回某对端的连接尝试顺序。
    pub fn connect_order(&self, device: &DeviceId) -> Vec<Candidate> {
        self.inner
            .lock()
            .unwrap()
            .get(device)
            .map(|p| p.connect_order())
            .unwrap_or_default()
    }

    /// 标记某地址连接成功（下次同类优先重试，加快重连）。
    pub fn mark_good(&self, device: &DeviceId, addr: &SocketAddr) {
        if let Some(p) = self.inner.lock().unwrap().get_mut(device) {
            p.mark_good(addr);
        }
    }

    /// 忘掉某对端的全部地址（解除配对时调用）。
    ///
    /// 不清理的话，拨号线程虽然因设备表里没它而不再拨号，地址簿里却还留着
    /// 一份陈旧记录，`clipsync addrs` 之类的诊断输出会显示一台早已解除配对的
    /// 设备，徒增困惑。
    pub fn forget(&self, device: &DeviceId) {
        self.inner.lock().unwrap().remove(device);
    }

    /// 某对端已知地址数量（诊断用）。
    pub fn count(&self, device: &DeviceId) -> usize {
        self.inner
            .lock()
            .unwrap()
            .get(device)
            .map(|p| p.len())
            .unwrap_or(0)
    }

    /// 全部对端及其候选（诊断展示用）。
    pub fn snapshot(&self) -> Vec<(DeviceId, Vec<Candidate>)> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|(d, p)| (d.clone(), p.connect_order()))
            .collect()
    }
}

impl Default for AddrBook {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev() -> DeviceId {
        DeviceId::from_public_key(b"peer")
    }

    #[test]
    fn adds_and_orders_addresses() {
        let book = AddrBook::new();
        let d = dev();
        book.add_addrs(
            &d,
            ["203.0.113.9:47684".parse().unwrap()],
            AddrSource::Manual,
        );
        book.add_addrs(
            &d,
            ["100.64.0.5:47684".parse().unwrap()],
            AddrSource::Pairing,
        );
        book.add_addrs(&d, ["127.0.0.1:47684".parse().unwrap()], AddrSource::Beacon);

        let order = book.connect_order(&d);
        assert_eq!(order.len(), 3);
        // 回环属于 LanDirect，应排最前；公网最后。
        assert_eq!(order[0].addr, "127.0.0.1:47684".parse().unwrap());
        assert_eq!(order[2].addr, "203.0.113.9:47684".parse().unwrap());
    }

    #[test]
    fn unusable_addresses_are_ignored() {
        let book = AddrBook::new();
        let d = dev();
        book.add_addrs(
            &d,
            ["169.254.9.9:47684".parse().unwrap()], // 链路本地
            AddrSource::Beacon,
        );
        assert_eq!(book.count(&d), 0);
    }

    #[test]
    fn duplicate_addresses_are_merged() {
        let book = AddrBook::new();
        let d = dev();
        let a: SocketAddr = "10.1.2.3:47684".parse().unwrap();
        book.add_addrs(&d, [a], AddrSource::Pairing);
        book.add_addrs(&d, [a], AddrSource::Beacon);
        assert_eq!(book.count(&d), 1);
    }

    #[test]
    fn mark_good_promotes_within_class() {
        let book = AddrBook::new();
        let d = dev();
        let first: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let second: SocketAddr = "10.0.0.2:1".parse().unwrap();
        book.add_addrs(&d, [first, second], AddrSource::Peer);
        book.mark_good(&d, &second);

        assert_eq!(book.connect_order(&d)[0].addr, second);
    }

    #[test]
    fn unknown_device_has_empty_order() {
        let book = AddrBook::new();
        assert!(book.connect_order(&dev()).is_empty());
    }
}
