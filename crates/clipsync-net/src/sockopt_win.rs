//! Windows 侧的套接字调优：qWAVE 后台流量声明、发送积压上限、保活。
//!
//! 与 macOS 侧（见父模块）目标一致、手段不同：
//!
//! - macOS 用 `SO_NET_SERVICE_TYPE=BK` 在**本机拥塞控制**里退让；Windows 的
//!   对应物是 qWAVE 的 `QOSTrafficTypeBackground`——系统的 QoS 调度器把这条流
//!   排在尽力而为流量之后，同时给包打上 DSCP CS1，支持 QoS 的路由器也会照办。
//!   典型受益场景恰好是本项目的核心用法：远程桌面连着这台机器时，剪贴板同步
//!   的大图片/文件不再挤占远程桌面的画面流量。
//! - macOS 用 `TCP_NOTSENT_LOWAT` 压内核积压；Windows 没有这个选项，改用
//!   `SO_SNDBUF` 上限。它比水位粗——连"在途"字节也一并封顶——代价是高延迟
//!   链路上的吞吐被限制在约 `SNDBUF_CAP / RTT`（512 KiB、50 ms 即 ~10 MB/s）。
//!   对后台流量这正是想要的取舍：局域网（毫秒级 RTT）不受影响，跨地域链路上
//!   慢一点换来"剪贴板变了立即中止"不被几 MB 的积压拖住。

use std::net::TcpStream;
use std::os::windows::io::AsRawSocket;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::NetworkManagement::QoS::{
    QOSAddSocketToFlow, QOSCreateHandle, QOSTrafficTypeBackground, QOS_NON_ADAPTIVE_FLOW,
    QOS_VERSION,
};
use windows_sys::Win32::Networking::WinSock::{
    setsockopt, IPPROTO_TCP, SOCKET, SOL_SOCKET, SO_KEEPALIVE, SO_SNDBUF, TCP_KEEPCNT,
    TCP_KEEPIDLE, TCP_KEEPINTVL, TCP_MAXRT,
};

/// 发送缓冲上限（bufferbloat 与中止延迟的封顶，取舍见模块说明）。
const SNDBUF_CAP: i32 = 512 * 1024;

/// 声明为"后台批量传输"并压住本机发送积压。
pub(super) fn background_bulk(stream: &TcpStream) {
    let sock = stream.as_raw_socket() as SOCKET;
    // SAFETY: sock 来自活着的 TcpStream，optval 为该选项要求的 i32。
    unsafe {
        setopt(sock, SOL_SOCKET, SO_SNDBUF, SNDBUF_CAP, "SO_SNDBUF");
    }
    if !add_to_background_flow(stream) {
        // qWAVE（QWAVE 服务）在个别 SKU 上不可用（如未装该功能的 Server）。
        // 此时只剩 SO_SNDBUF 那半边生效，文件传输会以普通优先级竞争带宽。
        tracing::debug!("声明后台流量失败（qWAVE 不可用），传输将以普通优先级竞争带宽");
    }
}

/// 把已连接的 socket 加入 qWAVE 的后台流量流。成功与否返回给调用方，
/// 测试据此断言（`setsockopt` 类失败都是静默的，只有断言能防它悄悄失效）。
fn add_to_background_flow(stream: &TcpStream) -> bool {
    let Some(handle) = qos_handle() else {
        return false;
    };
    let mut flow_id: u32 = 0; // 必须传 0：让系统新建一条流。
    // SAFETY: handle 由 QOSCreateHandle 返回且进程存活期内有效；socket 已连接，
    // 因此目的地址可传 NULL；flow_id 是本栈上的可写 u32。
    // 流会随 socket 关闭自动移除，无需配对调用 QOSRemoveSocketFromFlow。
    let ok = unsafe {
        QOSAddSocketToFlow(
            handle,
            stream.as_raw_socket() as SOCKET,
            std::ptr::null(),
            QOSTrafficTypeBackground,
            QOS_NON_ADAPTIVE_FLOW,
            &mut flow_id,
        )
    };
    if ok == 0 {
        tracing::debug!(
            "QOSAddSocketToFlow 失败（不影响功能）: {}",
            std::io::Error::last_os_error()
        );
        return false;
    }
    true
}

/// 进程级 qWAVE 句柄，首次使用时创建，进程退出前一直复用。
///
/// 失败（服务不可用）只发生一次并被记住，不会每条连接都重试一遍系统调用。
fn qos_handle() -> Option<HANDLE> {
    static BITS: OnceLock<usize> = OnceLock::new();
    let bits = *BITS.get_or_init(|| {
        let version = QOS_VERSION {
            MajorVersion: 1,
            MinorVersion: 0,
        };
        let mut handle: HANDLE = std::ptr::null_mut();
        // SAFETY: 两个指针都指向本栈上的有效对象。
        if unsafe { QOSCreateHandle(&version, &mut handle) } != 0 {
            handle as usize
        } else {
            tracing::debug!(
                "QOSCreateHandle 失败（不影响功能）: {}",
                std::io::Error::last_os_error()
            );
            0
        }
    });
    (bits != 0).then_some(bits as HANDLE)
}

/// 打开保活并给重传设期限（数值与理由见父模块常量）。
///
/// `TCP_KEEPIDLE`/`TCP_KEEPINTVL` 需要 Windows 10 1709+；更老的系统上这两条
/// 会静默失败，保活退回系统默认的 2 小时——不影响功能，只是死连接发现变慢。
pub(super) fn keepalive(stream: &TcpStream) {
    let sock = stream.as_raw_socket() as SOCKET;
    // SAFETY: sock 来自活着的 TcpStream，optval 均为各选项要求的 i32/DWORD。
    unsafe {
        setopt(sock, SOL_SOCKET, SO_KEEPALIVE, 1, "SO_KEEPALIVE");
        setopt(
            sock,
            IPPROTO_TCP,
            TCP_KEEPIDLE,
            super::KEEPALIVE_IDLE_SECS,
            "TCP_KEEPIDLE",
        );
        setopt(
            sock,
            IPPROTO_TCP,
            TCP_KEEPINTVL,
            super::KEEPALIVE_INTVL_SECS,
            "TCP_KEEPINTVL",
        );
        setopt(
            sock,
            IPPROTO_TCP,
            TCP_KEEPCNT,
            super::KEEPALIVE_CNT,
            "TCP_KEEPCNT",
        );
        // Windows 没有 macOS 的 TCP_RXT_CONNDROPTIME，等价物是 TCP_MAXRT：
        // 重传持续 N 秒无进展即放弃连接（DWORD，秒）。
        setopt(
            sock,
            IPPROTO_TCP,
            TCP_MAXRT,
            super::RETRANS_ABORT_SECS,
            "TCP_MAXRT",
        );
    }
}

/// 设一个 `i32` 型套接字选项，失败只记日志。
///
/// # Safety
/// `sock` 必须是一个有效的套接字。
unsafe fn setopt(sock: SOCKET, level: i32, name: i32, value: i32, what: &str) {
    let rc = setsockopt(
        sock,
        level,
        name,
        &value as *const i32 as *const u8,
        std::mem::size_of::<i32>() as i32,
    );
    if rc != 0 {
        tracing::debug!(
            "设置 {what} 失败（不影响功能）: {}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};
    use std::os::windows::io::AsRawSocket;

    use windows_sys::Win32::Networking::WinSock::{
        getsockopt, IPPROTO_TCP, SOCKET, SOL_SOCKET, SO_KEEPALIVE, SO_SNDBUF, TCP_KEEPIDLE,
    };

    /// 建一条已调优的回环连接。
    fn connected_and_tuned() -> TcpStream {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = std::thread::spawn(move || listener.accept());
        let c = TcpStream::connect(addr).unwrap();
        crate::sockopt::tune(&c);
        c
    }

    /// 读回一个 `i32` 型套接字选项。
    fn read_back(c: &TcpStream, level: i32, name: i32) -> i32 {
        let mut v: i32 = -1;
        let mut len = std::mem::size_of::<i32>() as i32;
        // SAFETY: socket 有效，缓冲区尺寸与 i32 匹配。
        let rc = unsafe {
            getsockopt(
                c.as_raw_socket() as SOCKET,
                level,
                name,
                &mut v as *mut i32 as *mut u8,
                &mut len,
            )
        };
        assert_eq!(rc, 0, "getsockopt 失败: {}", std::io::Error::last_os_error());
        v
    }

    /// 发送积压上限与保活必须**真的**设上——`setsockopt` 失败是静默的，
    /// 常量抄错或系统不支持时代码照样跑，只有读回来比一比才算数。
    #[test]
    fn backlog_cap_and_keepalive_are_really_applied() {
        let c = connected_and_tuned();
        assert_eq!(
            read_back(&c, SOL_SOCKET, SO_SNDBUF),
            super::SNDBUF_CAP,
            "发送缓冲上限"
        );
        assert_ne!(
            read_back(&c, SOL_SOCKET, SO_KEEPALIVE),
            0,
            "SO_KEEPALIVE 应已打开"
        );
        assert_eq!(
            read_back(&c, IPPROTO_TCP, TCP_KEEPIDLE),
            crate::sockopt::KEEPALIVE_IDLE_SECS,
            "保活空闲阈值（需要 Win10 1709+，CI 满足）"
        );
    }

    /// qWAVE 后台流声明必须成功——除非这台机器压根没有 qWAVE（未装该功能的
    /// Server SKU），那属于环境限制，跳过而不是失败。
    #[test]
    fn background_flow_is_added_when_qwave_exists() {
        let c = connected_and_tuned();
        if super::qos_handle().is_none() {
            eprintln!("跳过：本机没有 qWAVE（QWAVE 服务不可用）");
            return;
        }
        assert!(
            super::add_to_background_flow(&c),
            "qWAVE 可用时加入后台流不该失败"
        );
    }
}
