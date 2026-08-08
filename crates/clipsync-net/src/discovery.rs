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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
/// 常驻信标的组播间隔。
///
/// 它只负责"提示地址"——连接一旦建立，双方就改走加密通道互告地址，信标便
/// 无关紧要了。真正需要它的只有两个时刻：刚启动、以及 IP 变了之后重新被发现。
/// 这两件事都不需要秒级响应，5 秒偏于频繁：每台设备每分钟往局域网里丢 12 个
/// 组播包，N 台就是 12N，全天不停。放宽到 15 秒后降到三分之一，而"换了 Wi-Fi
/// 后最多 15 秒被重新发现"完全够用。
pub const BEACON_INTERVAL: Duration = Duration::from_secs(15);

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
    let target = SocketAddr::new(IpAddr::V4(BEACON_GROUP), BEACON_PORT);
    let handle = std::thread::Builder::new()
        .name("beacon-send".into())
        .spawn(move || {
            // 只在"全都发不出去 ↔ 恢复"这两个时刻记日志。原先每轮失败都记
            // 一条，多网卡机器上就是每 15 秒刷一行，把真正的问题淹没了。
            let mut healthy = true;
            loop {
                let beacon = Beacon::new(&device, sync_port, addrs_provider());
                match postcard::to_allocvec(&beacon) {
                    Ok(bytes) => {
                        let sent = send_multicast_on_all_ifaces(&bytes, target);
                        if sent == 0 && healthy {
                            warn!("{}", beacon_blocked_hint());
                            healthy = false;
                        } else if sent > 0 && !healthy {
                            info!("局域网信标已恢复（{sent} 个网卡）");
                            healthy = true;
                        }
                    }
                    Err(e) => warn!("编码信标失败: {e}"),
                }
                std::thread::sleep(BEACON_INTERVAL);
            }
        })?;
    Ok(handle)
}

/// 在**每个**可用网卡上各发一份组播，返回成功的网卡数。
///
/// **为什么不能只发一次**：绑到 `0.0.0.0` 时出接口由路由表决定，多网卡机器
/// 上多半不是你想要的那个。实机上 Mac mini 的默认路由走 utun，而 utun 不支持
/// 组播——于是日志里每 15 秒一条 `No route to host (os error 65)`，局域网发现
/// 从头到尾就没工作过。开发机上实测：
///
/// ```text
/// en0      192.168.2.177 -> ok
/// utun4    100.88.88.22  -> ok        （Tailscale，收下但不转发）
/// utun1024 198.18.0.1    -> No route to host   ← TUN 模式代理的假 IP 网卡
/// ```
///
/// 逐个网卡绑定源地址再发，既绕开了坏网卡，也让**所有**局域网口都真的收到
/// 广播。每轮重新枚举网卡（而不是启动时建好套接字）是为了跟上网络切换。
///
/// 用绑定源地址而不是 `IP_MULTICAST_IF`：后者标准库没有暴露，为它引入 libc
/// 或 socket2 不值当；BSD/Linux 上绑定具体源地址同样能选定出接口，上面那组
/// 实测就是证据。
fn send_multicast_on_all_ifaces(bytes: &[u8], target: SocketAddr) -> usize {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return 0;
    };
    let mut sent = 0;
    for ifa in ifaces {
        let if_addrs::IfAddr::V4(v4) = ifa.addr else {
            continue;
        };
        // 回环靠 set_multicast_loop_v4 就能送达同机监听者，不必单独发一份；
        // 链路本地地址（169.254/16）没有可用的对端。
        if v4.ip.is_loopback() || v4.ip.is_link_local() {
            continue;
        }
        let Ok(sock) = UdpSocket::bind((v4.ip, 0)) else {
            continue;
        };
        let _ = sock.set_multicast_loop_v4(true);
        if sock.send_to(bytes, target).is_ok() {
            sent += 1;
        }
    }
    sent
}

/// 组播发不出去时的提示。
///
/// 光说"发不出去"没法让人往下查。macOS 上最常见的原因不是网络，而是**本地
/// 网络权限**：系统把组播/广播归入该隐私类别，未授权时 `send_to` 直接返回
/// `EHOSTUNREACH`，看着像路由问题。同一份代码从终端跑却正常——终端自己有
/// 这个权限——所以这条线索必须写进日志，否则很难联想到。
fn beacon_blocked_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "局域网信标一个网卡都发不出去，局域网自动发现将不可用。\n         \
         macOS 上多半是本地网络权限未授权：请到\n         \
         「系统设置 › 隐私与安全性 › 本地网络」打开 ClipSync。\n         \
         （覆盖网如 Tailscale 不受影响，配对与同步仍可用）"
    } else {
        "局域网信标一个网卡都发不出去，局域网自动发现将不可用。\n         \
         请检查防火墙是否拦截了 UDP 组播（覆盖网不受影响）"
    }
}

/// 在**每个**网卡上加入组播组，返回成功的网卡数。
///
/// 与发送侧同一个道理：`join_multicast_v4(.., UNSPECIFIED)` 只在路由表选中的
/// 那个网卡上加入，别的网卡进来的信标一概收不到。一个都没成功时退回默认
/// 网卡，至少不比原先差。
fn join_multicast_on_all_ifaces(socket: &UdpSocket) -> usize {
    let mut joined = 0;
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for ifa in ifaces {
            let if_addrs::IfAddr::V4(v4) = ifa.addr else {
                continue;
            };
            if v4.ip.is_link_local() {
                continue;
            }
            if socket.join_multicast_v4(&BEACON_GROUP, &v4.ip).is_ok() {
                joined += 1;
            }
        }
    }
    if joined == 0
        && socket
            .join_multicast_v4(&BEACON_GROUP, &Ipv4Addr::UNSPECIFIED)
            .is_ok()
    {
        joined = 1;
    }
    joined
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
    if join_multicast_on_all_ifaces(&socket) == 0 {
        anyhow::bail!("加入组播组失败（所有网卡）");
    }
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
    ///
    /// **一台主机会有多个**：信标是逐网卡发送的，同一台机器从局域网口和
    /// 覆盖网口各来一份，源地址不同。按地址去重曾把这些当成"多台设备在
    /// 等待配对"而直接报错——同机 e2e 一跑就露馅。按设备名归并之后，多出来
    /// 的地址反倒是好事：一条走不通还能试下一条。
    pub addrs: Vec<SocketAddr>,
}

/// 配对宣告的运行句柄。**丢弃即停止宣告。**
///
/// 早先的实现返回 `JoinHandle` 并声明"句柄被丢弃时线程仍继续运行——配对进程
/// 本身是短命的，进程退出即停止"。这个假设只对 `clipsync pair --host` 这种
/// 一次性命令成立；**从托盘发起配对时进程是常驻的**，宣告线程就永远停不下来：
/// 用户看完配对码关掉窗口，局域网里仍在不停广播"我在等待配对"，每点一次菜单
/// 再多一个这样的线程。故改为 RAII 守卫。
pub struct PairingAnnouncer {
    stop: Arc<AtomicBool>,
}

impl Drop for PairingAnnouncer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// 在配对期间持续向局域网宣告"我在等待配对"。
///
/// 宣告在返回的 [`PairingAnnouncer`] 被丢弃时停止，因此调用方**必须持有**它
/// 直到配对结束——用 `let _ = ...` 接收会立即停止宣告。
pub fn spawn_pairing_announcer(device_name: String, pairing_port: u16) -> Result<PairingAnnouncer> {
    let target = SocketAddr::new(IpAddr::V4(BEACON_GROUP), PAIRING_BEACON_PORT);
    let beacon = PairingBeacon {
        magic: PAIRING_MAGIC,
        device_name,
        pairing_port,
    };
    let bytes = postcard::to_allocvec(&beacon).context("编码配对信标失败")?;

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    std::thread::Builder::new()
        .name("pair-announce".into())
        .spawn(move || {
            let mut warned = false;
            while !thread_stop.load(Ordering::SeqCst) {
                // 只在第一轮说一次：宣告每 800ms 一轮，每轮记一条会把日志刷爆
                // ——实机上就刷出了几十行一模一样的。
                if send_multicast_on_all_ifaces(&bytes, target) == 0 && !warned {
                    debug!("{}", beacon_blocked_hint());
                    warned = true;
                }
                // 分片 sleep：整段睡完才检查停止标志的话，停止最多要等一个
                // 完整间隔才生效，期间还会多广播一轮。
                sleep_interruptibly(PAIRING_BEACON_INTERVAL, &thread_stop);
            }
            debug!("配对宣告已停止");
        })?;

    Ok(PairingAnnouncer { stop })
}

/// 分片睡眠，期间发现停止标志就提前返回。
fn sleep_interruptibly(total: Duration, stop: &AtomicBool) {
    const SLICE: Duration = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < total {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let step = SLICE.min(total - slept);
        std::thread::sleep(step);
        slept += step;
    }
}

/// 在局域网中查找正在等待配对的设备。
///
/// 监听至多 `timeout`；一旦发现设备就提前返回（无需等满）。同一设备只返回一次。
pub fn discover_pairing_hosts(timeout: Duration) -> Result<Vec<PairingHost>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, PAIRING_BEACON_PORT))
        .with_context(|| format!("绑定配对发现端口 {PAIRING_BEACON_PORT} 失败"))?;
    if join_multicast_on_all_ifaces(&socket) == 0 {
        anyhow::bail!("加入组播组失败（所有网卡）");
    }
    // 分段等待，便于发现后尽早返回。
    socket
        .set_read_timeout(Some(Duration::from_millis(300)))
        .context("设置接收超时失败")?;

    let deadline = std::time::Instant::now() + timeout;
    let mut found: Vec<PairingHost> = Vec::new();
    let mut buf = [0u8; 1024];

    // 收一条信标，按**设备名**归并进结果；返回 true 表示这是第一次见到它。
    let absorb = |found: &mut Vec<PairingHost>, buf: &[u8], src: std::net::SocketAddr| {
        let Ok(b) = postcard::from_bytes::<PairingBeacon>(buf) else {
            return false; // 非本协议流量
        };
        if !b.is_valid() {
            return false;
        }
        let addr = SocketAddr::new(src.ip(), b.pairing_port);
        match found.iter_mut().find(|h| h.device_name == b.device_name) {
            Some(h) => {
                if !h.addrs.contains(&addr) {
                    h.addrs.push(addr);
                }
                false
            }
            None => {
                found.push(PairingHost {
                    device_name: b.device_name,
                    addrs: vec![addr],
                });
                true
            }
        }
    };

    while std::time::Instant::now() < deadline {
        let Ok((n, src)) = socket.recv_from(&mut buf) else {
            continue; // 超时或瞬时错误，继续等
        };
        if !absorb(&mut found, &buf[..n], src) {
            continue;
        }
        // 已找到设备，再稍等片刻，把它其余网卡的地址、以及可能存在的其它
        // 设备一并收齐。
        let grace = std::time::Instant::now() + Duration::from_millis(600);
        while std::time::Instant::now() < grace {
            if let Ok((n2, src2)) = socket.recv_from(&mut buf) {
                let b = buf;
                absorb(&mut found, &b[..n2], src2);
            }
        }
        break;
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_roundtrips_through_postcard() {
        let dev = DeviceId::from_public_key(b"test-device");
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

#[cfg(test)]
mod loopback_smoke {
    use super::*;

    /// 只跑发现，配合外部进程的 `clipsync pair --host` 定位收发哪一侧坏了。
    #[test]
    #[ignore = "需要外部进程在宣告"]
    fn manual_discover_only() {
        println!(
            "发现结果：{:?}",
            discover_pairing_hosts(Duration::from_secs(5))
        );
    }

    /// 逐网卡报告组播能力，用来判断"跳过"到底是环境限制还是我们的缺陷。
    ///
    /// 平时不跑（`#[ignore]`）。CI 上单独调起它，把结果发成注解——不然
    /// 只能看到一句"跳过：不支持组播"，凭什么这么说无从查证。
    #[test]
    #[ignore = "环境诊断，按需运行"]
    fn multicast_capability_report() {
        const PROBE_PORT: u16 = 47_698;
        let ifs = if_addrs::get_if_addrs().unwrap_or_default();
        println!("网卡：");
        for i in &ifs {
            if let if_addrs::IfAddr::V4(v4) = &i.addr {
                println!(
                    "  {:<12} {:<16} loopback={}",
                    i.name,
                    v4.ip,
                    i.is_loopback()
                );
            }
        }

        let Ok(rx) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, PROBE_PORT)) else {
            println!("绑定 {PROBE_PORT} 失败");
            return;
        };
        let mut joined = Vec::new();
        for i in &ifs {
            let if_addrs::IfAddr::V4(v4) = &i.addr else {
                continue;
            };
            if v4.ip.is_link_local() {
                continue;
            }
            match rx.join_multicast_v4(&BEACON_GROUP, &v4.ip) {
                Ok(()) => joined.push(format!("{}({})", i.name, v4.ip)),
                Err(e) => println!("  join 失败 {} {}: {e}", i.name, v4.ip),
            }
        }
        println!("join 成功：{joined:?}");

        let _ = rx.set_read_timeout(Some(Duration::from_millis(800)));
        let tx = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("绑定发送端失败");
        let _ = tx.set_multicast_loop_v4(true);
        let sent = tx.send_to(
            b"probe",
            SocketAddr::new(IpAddr::V4(BEACON_GROUP), PROBE_PORT),
        );
        let mut buf = [0u8; 32];
        let got = rx.recv_from(&mut buf).is_ok();
        println!("发送 ok={} / 自收 ok={}", sent.is_ok(), got);
        println!("结论：组播回环{}", if got { "可用" } else { "不可用" });
    }

    /// 这台机器现在能不能**自发自收**组播。
    ///
    /// CI runner（实测 GitHub 的 macOS runner）的网络沙箱里，组播组加得进去、
    /// 包却回不来。所以判据必须是真的发一份再收一份——只看 `join_multicast_v4`
    /// 是否成功会误判成"可用"，测试照样红。
    ///
    /// 用另一个端口探测，免得和真实信标端口上的监听者互相干扰。
    fn multicast_loopback_works() -> bool {
        const PROBE_PORT: u16 = 47_699;
        const MAGIC: &[u8] = b"clipsync-multicast-probe";

        let Ok(rx) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, PROBE_PORT)) else {
            return false;
        };
        if join_multicast_on_all_ifaces(&rx) == 0 {
            return false;
        }
        if rx
            .set_read_timeout(Some(Duration::from_millis(800)))
            .is_err()
        {
            return false;
        }

        let Ok(tx) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
            return false;
        };
        let _ = tx.set_multicast_loop_v4(true);
        if tx
            .send_to(MAGIC, SocketAddr::new(IpAddr::V4(BEACON_GROUP), PROBE_PORT))
            .is_err()
        {
            return false;
        }

        let mut buf = [0u8; 64];
        matches!(rx.recv_from(&mut buf), Ok((n, _)) if &buf[..n] == MAGIC)
    }

    /// 同机自发自收：宣告线程发出的配对信标，发现函数必须收得到，
    /// 且**多网卡来的多份必须算作一台设备**。
    ///
    /// 两条断言各挡一个真实故障：
    ///
    /// 一、能收到——"逐网卡收发"改造里，发送侧绑到某个网卡的源地址后，接收侧
    /// 必须在**同一个**网卡上加入过组播组，否则回环那份副本进不来，局域网
    /// 发现整个失效。
    ///
    /// 二、算作一台——这条是补写的。原先按**地址**去重，同一台机器从局域网口
    /// 和覆盖网口各发一份就成了"两台设备在等待配对"，调用方直接报错退出。
    /// 当时这个测试只断言"能找到"，于是绿着放过了；同机 e2e 一跑才露馅。
    /// 教训是断言要覆盖**调用方真正依赖的性质**，而不只是"有结果"。

    #[test]
    fn pairing_beacon_reaches_a_local_listener_as_one_host() {
        // 没有组播就没有可测的东西——这里跳过而不是失败。失败会把"环境没有
        // 这个能力"报成"功能坏了"，两者需要区分开：本机与 Windows runner 上
        // 它照常运行，真的回归了仍然抓得住。
        if !multicast_loopback_works() {
            eprintln!("跳过：本机网络不支持组播回环（CI 沙箱常见）");
            return;
        }
        let _ann = spawn_pairing_announcer("测试机".into(), 47_685).expect("启动宣告失败");
        let hosts = discover_pairing_hosts(Duration::from_secs(4)).expect("发现失败");

        let mine: Vec<_> = hosts.iter().filter(|h| h.device_name == "测试机").collect();
        assert_eq!(mine.len(), 1, "同一台机器只能算一台，实际 {hosts:?}");
        assert!(!mine[0].addrs.is_empty(), "总得给出至少一个可连地址");
        // 地址不去重的话，同一网卡的多轮信标会把列表撑爆。
        let mut uniq = mine[0].addrs.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), mine[0].addrs.len(), "地址不该重复");
    }
}
