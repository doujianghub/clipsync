//! 同步连接的套接字调优。
//!
//! 两件事：关掉 Nagle，以及**把这条连接声明成后台批量流量**。
//!
//! 后者是这个模块存在的理由。剪贴板同步在用户眼里是个后台差事，可它在网络
//! 上一点也不客气：一条不限速的 TCP 会一路加窗到把链路和沿途队列填满。用户
//! 这时候正在做的事——屏幕共享、视频通话、远程终端——全都排在那条队列后面，
//! 于是"复制了个文件"表现为"屏幕共享卡了"。
//!
//! 限速能治，但要用户猜一个数字，且猜不准：链路空着时限速是白白慢，链路忙时
//! 同一个数字又还是太快。正确的做法是让系统按**实时拥塞**决定，我们只需说清
//! 自己是什么流量。

use std::net::TcpStream;

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

/// 非 macOS 平台暂无等价能力。
///
/// Linux 有 `SO_PRIORITY`/`IP_TOS`，但那只是打标记，要沿途设备配合才有意义，
/// 家用网络里基本没人配；真正的退让得靠 LEDBAT 类拥塞控制，内核没有现成开关。
/// Windows 的等价物是 qWAVE（`QOSAddSocketToFlow` + `QOSTrafficTypeBackground`），
/// 有效但要额外维护 QoS 句柄的生命周期——等有人真的在 Windows 上撞到这个问题
/// 再说，眼下不为一个假想的需求加一层。
///
/// 这两个平台上文件传输仍会尽力占满链路，用户可用托盘里的「上传限速」兜底。
#[cfg(not(target_os = "macos"))]
fn background_bulk(_stream: &TcpStream) {}

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
        use std::os::unix::io::AsRawFd;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = std::thread::spawn(move || listener.accept());
        let c = TcpStream::connect(addr).unwrap();
        super::tune(&c);

        let read_back = |level, name| {
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
            assert_eq!(
                rc,
                0,
                "getsockopt 失败: {}",
                std::io::Error::last_os_error()
            );
            v
        };

        assert_eq!(read_back(libc::SOL_SOCKET, 0x1116), 1, "服务类型应为 BK(1)");
        assert_eq!(
            read_back(libc::IPPROTO_TCP, 0x201),
            128 * 1024,
            "未发送水位应为 128 KiB"
        );
    }

    /// Nagle 必须真的关掉——这一项在所有平台上都该成功，值得断言。
    #[test]
    fn nagle_is_actually_off() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = std::thread::spawn(move || listener.accept());
        let c = TcpStream::connect(addr).unwrap();
        super::tune(&c);
        assert!(c.nodelay().unwrap(), "TCP_NODELAY 应已启用");
    }
}
