//! `pairing_cli` 的单元测试。
//!
//! 单独成文件只为让 `pairing_cli.rs` 保持在项目约定的行数以内；
//! 它仍是 `pairing_cli` 的子模块（由 `#[path]` 引入），`use super::*`
//! 照常可用，与写在原文件里没有区别。

use super::*;

/// 回归：主持会话到期后必须**释放端口**。
///
/// 这是用户实际遇到的那个故障的核心：原实现 `loop { accept() }` 无超时
/// 无取消，用户关掉配对码窗口后线程永远卡在 accept 上，listener 永不析构，
/// 47685 被占死，再点菜单就是 `os error 10048`（Windows）/ `48`（macOS），
/// 只能重启程序。
///
/// 这里不走完整的 `host()`（它会生成配对码、起组播、等 3 分钟），只针对
/// 根因——`accept_until` 到期返回、listener 随之析构——做验证。
#[test]
fn accept_until_releases_port_after_deadline() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();

    let deadline = Instant::now() + Duration::from_millis(300);
    let got = accept_until(&listener, deadline).expect("轮询不应报错");
    assert!(got.is_none(), "无人连入时应到期返回 None，而不是永久阻塞");
    assert!(Instant::now() >= deadline, "应确实等到了截止时间");

    // 关键：会话结束后端口必须能被重新绑定。
    drop(listener);
    TcpListener::bind(("127.0.0.1", port))
        .expect("会话结束后端口应已释放——绑不上就意味着 10048 那个故障还在");
}

/// 到期前有人连入时应正常返回连接，且流已恢复为阻塞模式。
///
/// 非阻塞 listener 接出来的流默认也是非阻塞的，直接拿去跑同步握手会
/// 一路撞 `WouldBlock`——这条守的就是那个转换没被漏掉。
#[test]
fn accept_until_returns_connection_in_blocking_mode() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    // 客户端**延迟**再发数据：服务端必然先进入 read。
    // 阻塞流会一直等到数据到达；非阻塞流则立刻 WouldBlock 报错。
    // （不能用"读超时是否返回 WouldBlock"来区分——超时在 macOS 上返回的
    //  同样是 WouldBlock，两种情况分不开。）
    let client = std::thread::spawn(move || {
        use std::io::Write;
        let mut s = TcpStream::connect(addr).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        s.write_all(b"X").unwrap();
        s
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let (stream, _) = accept_until(&listener, deadline)
        .expect("不应报错")
        .expect("应接到连接");

    let mut buf = [0u8; 1];
    std::io::Read::read_exact(&mut &stream, &mut buf)
        .expect("流仍是非阻塞模式——同步握手会一路撞 WouldBlock");
    assert_eq!(&buf, b"X");

    let _ = client.join();
}

/// 手动目视验证：弹出与托盘「显示配对码」**完全一致**的窗口。
///
/// 这里刻意复用真实的 `dialog_body` 与 `show_info_and_copy`，而不是另写
/// 一段相似的内容——照抄一遍只能证明抄得对，证明不了线上那条路径对。
/// 唯一省略的是 `TcpListener` 与 `accept` 循环：它们与"窗口显示成什么样"
/// 无关，却会让测试永久阻塞。
///
/// 跑法：`cargo test -p clipsync-app --bin clipsync -- --ignored pairing_dialog`
///
/// 判据：窗口标题为「ClipSync 配对」，正文首行的配对码可读，
/// 且该配对码已进入剪贴板（正文里"已复制到剪贴板"这句得是真的）。
#[test]
#[ignore = "会弹窗并阻塞，需人工/脚本关闭"]
fn manual_pairing_dialog() {
    let code = PairingCode::generate();
    // 打到 stdout，供外部脚本比对窗口里显示的是不是同一个码。
    println!("EXPECT_CODE={code}");
    crate::dialog::show_info_and_copy(
        "ClipSync 配对",
        &dialog_body(&code, 47_684, true),
        &code.to_string(),
    );
}
