//! 同步连接的套接字调优。
//!
//! 三件事：关掉 Nagle、**把这条连接声明成后台批量流量**、以及**给死连接一个
//! 发现期限**。
//!
//! 声明后台流量是这个模块存在的理由。剪贴板同步在用户眼里是个后台差事，可它
//! 在网络上一点也不客气：一条不限速的 TCP 会一路加窗到把链路和沿途队列填满。
//! 用户这时候正在做的事——屏幕共享、视频通话、远程桌面——全都排在那条队列
//! 后面，于是"复制了个文件"表现为"屏幕共享卡了"。
//!
//! 限速能治，但要用户猜一个数字，且猜不准：链路空着时限速是白白慢，链路忙时
//! 同一个数字又还是太快。正确的做法是让系统按**实时拥塞**决定，我们只需说清
//! 自己是什么流量。macOS 与 Windows 的做法不同（服务类型 vs qWAVE），各自见
//! 平台实现。
//!
//! 死连接期限解决的是另一个问题：对端不辞而别（合盖睡眠、换 Wi-Fi、VPN 断开）
//! 时 TCP 不会有任何通知，收发泵的读超时只会一轮轮正常超时，写出去的字节则由
//! 内核默默重传**十几分钟**才报错。这期间 `ConnRegistry` 里还挂着旧连接，对端
//! 醒来后拨进来的新连接会被当成重复连接关掉——两台明明都在线的设备就这么
//! 互相干瞪眼。协议里的 `Ping`/`Pong` 从没人发过，保活这件事内核本来就会做，
//! 只是默认周期（2 小时）等于没有，把它调到分钟级即可。

use std::net::TcpStream;

/// 空闲多少秒后开始发保活探测。
///
/// 与 `KEEPALIVE_INTVL_SECS`/`KEEPALIVE_CNT` 合起来：链路断了约 60 秒内读到
/// 错误、泵退出、`ConnRegistry` 释放，对端重连不再被挡。取 30 秒起步是因为
/// Wi-Fi 漫游、AP 切换这类几秒钟的抖动不该杀掉一条健康连接。
#[cfg(any(target_os = "macos", windows))]
const KEEPALIVE_IDLE_SECS: i32 = 30;
/// 保活探测无应答后，隔多少秒再探一次。
#[cfg(any(target_os = "macos", windows))]
const KEEPALIVE_INTVL_SECS: i32 = 10;
/// 连续多少次探测无应答判定连接已死。
#[cfg(any(target_os = "macos", windows))]
const KEEPALIVE_CNT: i32 = 3;
/// 有数据待重传时，持续这么多秒毫无进展（一个 ACK 都等不到）就放弃连接。
///
/// 保活只管**空闲**连接；一旦有未确认数据（比如每 60 秒的地址通告写进了死链
/// 路），走的是重传路径，默认要指数退避到十几分钟才放弃。健康网络上连续
/// 30 秒收不到任何 ACK 只有链路断了一种解释——真在重传的慢链路每轮都会
/// 收到 ACK，不会触发这个期限。
#[cfg(any(target_os = "macos", windows))]
const RETRANS_ABORT_SECS: i32 = 30;

/// 调整套接字参数以适配本协议的收发模式。
///
/// 全部为"尽力而为"：任何一项失败都只记日志。这些是优化，不是正确性依赖，
/// 在不支持的平台/虚拟网卡上退回默认行为即可。
pub fn tune(stream: &TcpStream) {
    // **关闭 Nagle**：本协议是"一帧接一帧"的流式模式，Nagle 会为了合并小包而
    // 等待对端 ACK；与接收端的延迟确认叠加时，可能造成数十毫秒的停顿。我们
    // 自己已经把长度前缀与负载合并成单次写出，不需要内核再代为合并。
    if let Err(e) = stream.set_nodelay(true) {
        tracing::debug!("设置 TCP_NODELAY 失败（不影响功能，可能略增延迟）: {e}");
    }
    keepalive(stream);
    background_bulk(stream);
}

/// 声明为"后台批量传输"，让系统在拥塞时优先保障交互流量。
#[cfg(target_os = "macos")]
fn background_bulk(stream: &TcpStream) {
    use std::os::unix::io::AsRawFd;

    // 这几个常量 libc crate 尚未导出，取自 SDK 头文件：
    //   sys/socket.h:  SO_NET_SERVICE_TYPE 0x1116, NET_SERVICE_TYPE_BK 1
    //   netinet/tcp.h: TCP_NOTSENT_LOWAT   0x201
    const SO_NET_SERVICE_TYPE: libc::c_int = 0x1116;
    const NET_SERVICE_TYPE_BK: libc::c_int = 1;
    const TCP_NOTSENT_LOWAT: libc::c_int = 0x201;

    // 内核里"尚未发出"的数据水位。
    //
    // macOS 的发送缓冲会自动涨到几 MB。在一条 3 MB/s 的链路上那是**好几秒**的
    // 积压：`write` 早就返回了，字节却还堵在本机，屏幕共享的包排在它们后面。
    // 更糟的是"剪贴板变了，中止这次传输"根本来不及——几 MB 已经在路上。
    //
    // 压到 128 KiB：既保证内核手里始终有活干（不会因为等应用喂数据而断流），
    // 又把本机侧的排队延迟限制在一个分块的量级。这是**本机能控制的那半个
    // bufferbloat**，另外半个在路由器上，交给下面的 BK 让路去处理。
    const UNSENT_LOWAT: libc::c_int = 128 * 1024;

    let fd = stream.as_raw_fd();
    // SAFETY: fd 来自活着的 TcpStream，两个 optval 都是按各自选项要求的 c_int。
    unsafe {
        // 「后台、可容忍高延迟、长时间的批量流」——SDK 头文件给这一类举的例子
        // 就是 "synching or backup"，正是我们。而屏幕共享属于 NET_SERVICE_TYPE_RV，
        // 排在我们前面。系统据此在拥塞时先让交互流量走。
        //
        // 注意这不改 DSCP（那是 BK_SYS 的事），沿途设备不需要配合；退让发生在
        // 本机的拥塞控制里，所以在任何网络上都有效。
        setopt(
            fd,
            libc::SOL_SOCKET,
            SO_NET_SERVICE_TYPE,
            NET_SERVICE_TYPE_BK,
            "SO_NET_SERVICE_TYPE=BK",
        );
        setopt(
            fd,
            libc::IPPROTO_TCP,
            TCP_NOTSENT_LOWAT,
            UNSENT_LOWAT,
            "TCP_NOTSENT_LOWAT",
        );
    }
}

/// 打开保活并给重传设期限，让死连接在约一分钟内暴露（理由见模块说明）。
#[cfg(target_os = "macos")]
fn keepalive(stream: &TcpStream) {
    use std::os::unix::io::AsRawFd;

    // libc 尚未导出，取自 netinet/tcp.h：重传持续 N 秒无进展即放弃连接。
    const TCP_RXT_CONNDROPTIME: libc::c_int = 0x80;

    let fd = stream.as_raw_fd();
    // SAFETY: fd 来自活着的 TcpStream，optval 均为各选项要求的 c_int。
    unsafe {
        setopt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1, "SO_KEEPALIVE");
        // macOS 的"空闲阈值"叫 TCP_KEEPALIVE（Linux 上才叫 TCP_KEEPIDLE）。
        setopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_KEEPALIVE,
            KEEPALIVE_IDLE_SECS,
            "TCP_KEEPALIVE",
        );
        setopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_KEEPINTVL,
            KEEPALIVE_INTVL_SECS,
            "TCP_KEEPINTVL",
        );
        setopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_KEEPCNT,
            KEEPALIVE_CNT,
            "TCP_KEEPCNT",
        );
        setopt(
            fd,
            libc::IPPROTO_TCP,
            TCP_RXT_CONNDROPTIME,
            RETRANS_ABORT_SECS,
            "TCP_RXT_CONNDROPTIME",
        );
    }
}

/// 设一个 `c_int` 型套接字选项，失败只记日志。
///
/// # Safety
/// `fd` 必须是一个有效的套接字描述符。
#[cfg(target_os = "macos")]
unsafe fn setopt(
    fd: libc::c_int,
    level: libc::c_int,
    name: libc::c_int,
    value: libc::c_int,
    what: &str,
) {
    let rc = libc::setsockopt(
        fd,
        level,
        name,
        &value as *const libc::c_int as *const libc::c_void,
        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
    );
    if rc != 0 {
        tracing::debug!(
            "设置 {what} 失败（不影响功能）: {}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(windows)]
#[path = "sockopt_win.rs"]
mod win;

#[cfg(windows)]
use win::{background_bulk, keepalive};

/// 其余平台（目前只剩 Linux，非发布目标）暂无等价实现。
///
/// Linux 有现成的对应物（TCP_NOTSENT_LOWAT、TCP_KEEPIDLE、TCP_USER_TIMEOUT、
/// SO_PRIORITY），但 CI 不编译 Linux 目标，写了也是一段没人验证的死代码——
/// 等真有 Linux 发布需求时随 CI 一起加。此前文件传输在 Linux 上仍会尽力占满
/// 链路，用户可用托盘里的「上传限速」兜底。
#[cfg(not(any(target_os = "macos", windows)))]
fn background_bulk(_stream: &TcpStream) {}

#[cfg(not(any(target_os = "macos", windows)))]
fn keepalive(_stream: &TcpStream) {}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};

    /// 调优对一条真实连接必须是无害的：设完仍能正常收发。
    ///
    /// 不断言 `setsockopt` 的返回值——它在容器/虚拟网卡上失败是允许的，本模块
    /// 的契约就是"失败也不影响功能"。这里要守住的正是后半句。
    #[test]
    fn tuning_keeps_the_connection_usable() {
        use std::io::{Read, Write};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            super::tune(&s);
            let mut buf = [0u8; 5];
            s.read_exact(&mut buf).unwrap();
            s.write_all(&buf).unwrap();
        });

        let mut c = TcpStream::connect(addr).unwrap();
        super::tune(&c);
        c.write_all(b"hello").unwrap();
        let mut back = [0u8; 5];
        c.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"hello");
        server.join().unwrap();
    }

    /// macOS 上必须**真的**把这条连接标成后台流量。
    ///
    /// 为什么值得单独断言：`setsockopt` 失败是静默的（只记 debug 日志），而这
    /// 两个常量是从 SDK 头文件里抄来的字面量，不是 libc 导出的符号。抄错一个
    /// 数字、或者将来 Apple 改了值，代码照样编译、照样跑、照样什么都不做——
    /// 症状是"屏幕共享还是卡"，没人会想到来看这里。读回来比一比才算数。
    #[cfg(target_os = "macos")]
    #[test]
    fn background_class_is_really_applied() {
        let c = connected_and_tuned();
        assert_eq!(
            read_back(&c, libc::SOL_SOCKET, 0x1116),
            1,
            "服务类型应为 BK(1)"
        );
        assert_eq!(
            read_back(&c, libc::IPPROTO_TCP, 0x201),
            128 * 1024,
            "未发送水位应为 128 KiB"
        );
    }

    /// 保活与重传期限也一样：常量是字面量，只有读回来才知道真的生效了。
    /// 少了它们，死连接要靠十几分钟的重传超时才暴露，期间挡着对端重连。
    #[cfg(target_os = "macos")]
    #[test]
    fn dead_link_detection_is_really_applied() {
        let c = connected_and_tuned();
        assert_ne!(
            read_back(&c, libc::SOL_SOCKET, libc::SO_KEEPALIVE),
            0,
            "SO_KEEPALIVE 应已打开"
        );
        assert_eq!(
            read_back(&c, libc::IPPROTO_TCP, libc::TCP_KEEPALIVE),
            super::KEEPALIVE_IDLE_SECS,
            "保活空闲阈值"
        );
        assert_eq!(
            read_back(&c, libc::IPPROTO_TCP, 0x80),
            super::RETRANS_ABORT_SECS,
            "重传放弃期限"
        );
    }

    /// Nagle 必须真的关掉——这一项在所有平台上都该成功，值得断言。
    #[test]
    fn nagle_is_actually_off() {
        let c = connected_and_tuned();
        assert!(c.nodelay().unwrap(), "TCP_NODELAY 应已启用");
    }

    /// 建一条已调优的回环连接（监听端由游离线程收下即可）。
    fn connected_and_tuned() -> TcpStream {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = std::thread::spawn(move || listener.accept());
        let c = TcpStream::connect(addr).unwrap();
        super::tune(&c);
        c
    }

    /// 读回一个 `c_int` 型套接字选项。
    #[cfg(target_os = "macos")]
    fn read_back(c: &TcpStream, level: libc::c_int, name: libc::c_int) -> libc::c_int {
        use std::os::unix::io::AsRawFd;

        let mut v: libc::c_int = -1;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: fd 有效，缓冲区尺寸与 c_int 匹配。
        let rc = unsafe {
            libc::getsockopt(
                c.as_raw_fd(),
                level,
                name,
                &mut v as *mut libc::c_int as *mut libc::c_void,
                &mut len,
            )
        };
        assert_eq!(rc, 0, "getsockopt 失败: {}", std::io::Error::last_os_error());
        v
    }
}
