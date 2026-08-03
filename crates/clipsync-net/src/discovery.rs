//! 局域网设备发现：UDP 组播信标。
//!
//! **为何自研而非用 mDNS 库**：需求很窄——在同一局域网内周期性宣告"我是设备 X，
//! 同步端口是 P，我还有这些地址"。自研信标只需 ~150 行纯 std 代码，无额外依赖、
//! 无后台服务、内存开销可忽略，且 payload 可直接携带我们需要的候选地址表
//! （mDNS 的 TXT 记录反而更别扭）。契合"低占用、稳定简便"。
//!
//! **协议**：向组播组 239.255.71.83:47690 周期性发送 postcard 编码的
//! [`Beacon`]；同时监听该组播组接收他人的信标。收到信标后，若其 device_id 属于
//! 已配对设备，则把"信标源 IP + 其宣告的同步端口"以及其携带的其它地址作为候选。
//!
//! **安全性**：信标不做认证，任何人都能伪造。这不构成风险——伪造只会导致一次
//! 失败的连接尝试，因为随后的 Noise_IK 握手需要对端的静态私钥才能通过。因此
//! 信标只用于"提示地址"，认证始终由传输层负责。
//!
//! **跨网络场景**：信标仅覆盖局域网。异地组网（Tailscale/ZeroTier/WireGuard 等）
//! 的地址通过另外两条通用途径获得：配对时交换、以及连接后经加密通道持续同步
//! （见 [`crate::peer`] 与 `SyncMessage::Addresses`）。

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use anyhow::{Context, Result};
use clipsync_core::DeviceId;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// 信标组播组（管理范围组播地址，不会跨路由器外泄）。
pub const BEACON_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 71, 83);
/// 信标端口。
pub const BEACON_PORT: u16 = 47_690;
/// 信标发送间隔。
pub const BEACON_INTERVAL: Duration = Duration::from_secs(5);

/// **配对**专用的发现端口。
///
/// 刻意与 [`BEACON_PORT`] 分开：常驻守护进程会长期占用 47690，而配对是独立
/// 的短命进程；用不同端口就不会互相抢占，用户可以在守护进程运行时随时发起配对。
pub const PAIRING_BEACON_PORT: u16 = 47_691;
/// 配对期间的宣告间隔（比常规信标频繁，让对方尽快发现）。
pub const PAIRING_BEACON_INTERVAL: Duration = Duration::from_millis(800);

/// 信标包魔数与版本，防止误解析其它协议的组播流量。
const MAGIC: [u8; 4] = *b"CSY1";
/// 配对信标的魔数，与常规信标区分。
const PAIRING_MAGIC: [u8; 4] = *b"CSYP";

/// 一条信标：宣告本机身份与可达地址。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Beacon {
    magic: [u8; 4],
    /// 设备 ID（静态公钥指纹的十六进制串）。
    pub device: String,
    /// 同步服务监听端口。
    pub sync_port: u16,
    /// 本机其它可达地址（覆盖网/公网等），供对端在离开局域网后仍可尝试。
    pub addrs: Vec<SocketAddr>,
}

impl Beacon {
    pub fn new(device: &DeviceId, sync_port: u16, addrs: Vec<SocketAddr>) -> Self {
        Self {
            magic: MAGIC,
            device: device.to_string(),
            sync_port,
            addrs,
        }
    }

    fn is_valid(&self) -> bool {
        self.magic == MAGIC
    }
}

/// 发现到的对端地址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub device: DeviceId,
    /// 信标源地址推断出的局域网地址（源 IP + 其宣告的同步端口）。
    pub lan_addr: SocketAddr,
    /// 对端宣告的其它地址。
    pub extra_addrs: Vec<SocketAddr>,
}

/// 启动信标发送线程：周期性向组播组宣告本机。
///
/// 使用临时端口发送，因此同一台机器上运行多个实例也不会冲突。
pub fn spawn_sender(
    device: DeviceId,
    sync_port: u16,
    // 每轮发送前重新求值，使地址变化（如 VPN 上线）能及时反映。
    addrs_provider: impl Fn() -> Vec<SocketAddr> + Send + 'static,
) -> Result<std::thread::JoinHandle<()>> {
    // 绑定临时端口发送，避免与监听端口争用。
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).context("绑定信标发送套接字失败")?;
    socket
        .set_multicast_loop_v4(true)
        .context("设置组播回环失败")?;

    let target = SocketAddr::new(IpAddr::V4(BEACON_GROUP), BEACON_PORT);
    let handle = std::thread::Builder::new()
        .name("beacon-send".into())
        .spawn(move || loop {
            let beacon = Beacon::new(&device, sync_port, addrs_provider());
            match postcard::to_allocvec(&beacon) {
                Ok(bytes) => {
                    if let Err(e) = socket.send_to(&bytes, target) {
                        debug!("发送信标失败（可能无可用网络）: {e}");
                    }
                }
                Err(e) => warn!("编码信标失败: {e}"),
            }
            std::thread::sleep(BEACON_INTERVAL);
        })?;
    Ok(handle)
}

/// 启动信标监听线程：接收组播信标并回调。
///
/// 端口被占用时（如同机第二个实例）返回错误，调用方可降级为不使用局域网发现
/// ——此时仍可通过配对时交换的地址与对端通告的地址连接。
pub fn spawn_listener(
    self_device: DeviceId,
    mut on_found: impl FnMut(Discovered) + Send + 'static,
) -> Result<std::thread::JoinHandle<()>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, BEACON_PORT))
        .with_context(|| format!("绑定信标端口 {BEACON_PORT} 失败"))?;
    socket
        .join_multicast_v4(&BEACON_GROUP, &Ipv4Addr::UNSPECIFIED)
        .context("加入组播组失败")?;
    info!("局域网发现已启动（组播 {BEACON_GROUP}:{BEACON_PORT}）");

    let handle = std::thread::Builder::new()
        .name("beacon-recv".into())
        .spawn(move || {
            let mut buf = [0u8; 2048];
            loop {
                let (n, src) = match socket.recv_from(&mut buf) {
                    Ok(v) => v,
                    Err(e) => {
                        debug!("接收信标失败: {e}");
                        std::thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                };
                let beacon: Beacon = match postcard::from_bytes(&buf[..n]) {
                    Ok(b) => b,
                    Err(_) => continue, // 非本协议流量，静默忽略
                };
                if !beacon.is_valid() {
                    continue;
                }
                let device = DeviceId::from_hex(beacon.device.clone());
                // 忽略自己的信标（组播回环会收到）。
                if device == self_device {
                    continue;
                }
                on_found(Discovered {
                    device,
                    lan_addr: SocketAddr::new(src.ip(), beacon.sync_port),
                    extra_addrs: beacon.addrs,
                });
            }
        })?;
    Ok(handle)
}

// ———————————————————————————————————————————————————————————————
// 配对发现：让"加入方"无需手输 IP
// ———————————————————————————————————————————————————————————————

/// 一台正在等待配对的设备所宣告的信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingBeacon {
    magic: [u8; 4],
    /// 主持方设备名，供用户确认连的是不是自己那台机器。
    pub device_name: String,
    /// 主持方的配对监听端口。
    pub pairing_port: u16,
}

impl PairingBeacon {
    fn is_valid(&self) -> bool {
        self.magic == PAIRING_MAGIC
    }
}

/// 发现到的一台待配对设备。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingHost {
    pub device_name: String,
    /// 可直接连接的配对地址（信标来源 IP + 其宣告的配对端口）。
    pub addr: SocketAddr,
}

/// 在配对期间持续向局域网宣告"我在等待配对"。
///
/// 返回的句柄被丢弃时线程仍继续运行——配对进程本身是短命的，进程退出即停止。
pub fn spawn_pairing_announcer(
    device_name: String,
    pairing_port: u16,
) -> Result<std::thread::JoinHandle<()>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).context("绑定配对宣告套接字失败")?;
    socket
        .set_multicast_loop_v4(true)
        .context("设置组播回环失败")?;

    let target = SocketAddr::new(IpAddr::V4(BEACON_GROUP), PAIRING_BEACON_PORT);
    let beacon = PairingBeacon {
        magic: PAIRING_MAGIC,
        device_name,
        pairing_port,
    };
    let bytes = postcard::to_allocvec(&beacon).context("编码配对信标失败")?;

    let handle = std::thread::Builder::new()
        .name("pair-announce".into())
        .spawn(move || loop {
            if let Err(e) = socket.send_to(&bytes, target) {
                debug!("发送配对信标失败: {e}");
            }
            std::thread::sleep(PAIRING_BEACON_INTERVAL);
        })?;
    Ok(handle)
}

/// 在局域网中查找正在等待配对的设备。
///
/// 监听至多 `timeout`；一旦发现设备就提前返回（无需等满）。同一设备只返回一次。
pub fn discover_pairing_hosts(timeout: Duration) -> Result<Vec<PairingHost>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, PAIRING_BEACON_PORT))
        .with_context(|| format!("绑定配对发现端口 {PAIRING_BEACON_PORT} 失败"))?;
    socket
        .join_multicast_v4(&BEACON_GROUP, &Ipv4Addr::UNSPECIFIED)
        .context("加入组播组失败")?;
    // 分段等待，便于发现后尽早返回。
    socket
        .set_read_timeout(Some(Duration::from_millis(300)))
        .context("设置接收超时失败")?;

    let deadline = std::time::Instant::now() + timeout;
    let mut found: Vec<PairingHost> = Vec::new();
    let mut buf = [0u8; 1024];

    while std::time::Instant::now() < deadline {
        let (n, src) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(_) => continue, // 超时或瞬时错误，继续等
        };
        let beacon: PairingBeacon = match postcard::from_bytes(&buf[..n]) {
            Ok(b) => b,
            Err(_) => continue, // 非本协议流量
        };
        if !beacon.is_valid() {
            continue;
        }
        let host = PairingHost {
            device_name: beacon.device_name,
            addr: SocketAddr::new(src.ip(), beacon.pairing_port),
        };
        if !found.iter().any(|h| h.addr == host.addr) {
            found.push(host);
            // 已找到设备，再稍等片刻收集可能的其它设备后返回。
            let grace = std::time::Instant::now() + Duration::from_millis(600);
            while std::time::Instant::now() < grace {
                if let Ok((n2, src2)) = socket.recv_from(&mut buf) {
                    if let Ok(b2) = postcard::from_bytes::<PairingBeacon>(&buf[..n2]) {
                        if b2.is_valid() {
                            let h2 = PairingHost {
                                device_name: b2.device_name,
                                addr: SocketAddr::new(src2.ip(), b2.pairing_port),
                            };
                            if !found.iter().any(|h| h.addr == h2.addr) {
                                found.push(h2);
                            }
                        }
                    }
                }
            }
            break;
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_roundtrips_through_postcard() {        let dev = DeviceId::from_public_key(b"test-device");
        let b = Beacon::new(
            &dev,
            47684,
            vec![
                "100.101.102.103:47684".parse().unwrap(),
                "192.168.1.7:47684".parse().unwrap(),
            ],
        );
        let bytes = postcard::to_allocvec(&b).unwrap();
        let back: Beacon = postcard::from_bytes(&bytes).unwrap();

        assert!(back.is_valid());
        assert_eq!(back.device, dev.to_string());
        assert_eq!(back.sync_port, 47684);
        assert_eq!(back.addrs.len(), 2);
    }

    #[test]
    fn beacon_with_wrong_magic_is_invalid() {
        let dev = DeviceId::from_public_key(b"x");
        let mut b = Beacon::new(&dev, 1, vec![]);
        b.magic = *b"XXXX";
        assert!(!b.is_valid());
    }

    /// 随机字节不应被误解析为有效信标（魔数与结构双重保护）。
    #[test]
    fn random_bytes_do_not_parse_as_valid_beacon() {
        let junk = [0xABu8; 64];
        match postcard::from_bytes::<Beacon>(&junk) {
            Ok(b) => assert!(!b.is_valid(), "随机数据不应通过魔数校验"),
            Err(_) => { /* 解析失败也是预期结果 */ }
        }
    }

    /// 信标包应足够小，避免 UDP 分片（典型 MTU 1500）。
    #[test]
    fn beacon_packet_stays_small() {
        let dev = DeviceId::from_public_key(b"device");
        let addrs: Vec<SocketAddr> = (0..8)
            .map(|i| format!("10.0.0.{i}:47684").parse().unwrap())
            .collect();
        let b = Beacon::new(&dev, 47684, addrs);
        let bytes = postcard::to_allocvec(&b).unwrap();
        assert!(bytes.len() < 1200, "信标包过大: {} 字节", bytes.len());
    }
}
