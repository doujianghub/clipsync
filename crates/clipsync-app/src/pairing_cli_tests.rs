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
/// 判据：窗口标题为「ClipSync 配对」，正文首行的「码@地址」可读，
/// 且**这一整串**已进入剪贴板（正文里"已复制到剪贴板"这句得是真的）。
#[test]
#[ignore = "会弹窗并阻塞，需人工/脚本关闭"]
fn manual_pairing_dialog() {
    let share = share_string(&PairingCode::generate(), 47_684);
    // 打到 stdout，供外部脚本比对窗口里显示的是不是同一串。
    println!("EXPECT_CODE={share}");
    crate::dialog::show_info_and_copy("ClipSync 配对", &dialog_body(&share, true), &share);
}

/// 纯配对码：不带地址，调用方走局域网自动发现。
#[test]
fn parses_bare_code() {
    let (code, host) = parse_pairing_input("ABCDEF").expect("应识别为配对码");
    assert_eq!(code.as_str(), "ABCDEF");
    assert!(host.is_none(), "没带地址就该让调用方去自动发现");
}

/// 带地址的串：跨网络配对的正路。
///
/// Tailscale 这类覆盖网不转发组播，自动发现注定落空——地址必须随码带过来，
/// 否则用户得先看一屏 IP 再手敲。
#[test]
fn parses_code_with_address() {
    let (code, host) = parse_pairing_input("ABCDEF@100.88.88.22").unwrap();
    assert_eq!(code.as_str(), "ABCDEF");
    assert_eq!(host.as_deref(), Some("100.88.88.22"));
}

/// IPv6 要从右往左切：地址里全是冒号，但不会有 @。
#[test]
fn parses_ipv6_address() {
    let (code, host) = parse_pairing_input("ABCDEF@[fd7a:115c:a1e0::e201:c839]").unwrap();
    assert_eq!(code.as_str(), "ABCDEF");
    assert_eq!(host.as_deref(), Some("[fd7a:115c:a1e0::e201:c839]"));
}

/// 用户输入不会规整：大小写、空格、连字符都得认（沿用 PairingCode::parse）。
#[test]
fn tolerates_messy_input() {
    assert_eq!(parse_pairing_input("  abcdef  ").unwrap().0.as_str(), "ABCDEF");
    assert_eq!(parse_pairing_input("abc-def").unwrap().0.as_str(), "ABCDEF");
    let (c, h) = parse_pairing_input(" abcdef@10.0.0.5 ").unwrap();
    assert_eq!((c.as_str(), h.as_deref()), ("ABCDEF", Some("10.0.0.5")));
}

#[test]
fn rejects_malformed_input() {
    assert!(parse_pairing_input("").is_none());
    assert!(parse_pairing_input("TOOLONGCODE").is_none());
    assert!(parse_pairing_input("ABCDEF@").is_none(), "@ 后面空着不算有效地址");
    // 字符集里没有 0/O/1/I/L，避免手抄时混淆。
    assert!(parse_pairing_input("ABC0EF").is_none());
}

/// 生成的串必须能被自己解析回去——两边写法一旦不一致，用户复制粘贴就失败。
#[test]
fn generated_string_round_trips() {
    let code = PairingCode::from_entropy(b"ABCDEF");
    for addr in ["192.168.1.5:47684", "[fd7a:115c:a1e0::1]:47684"] {
        let sa: std::net::SocketAddr = addr.parse().unwrap();
        let s = format_pairing_string(&code, &sa);
        let (back, host) = parse_pairing_input(&s)
            .unwrap_or_else(|| panic!("自己生成的串应能解析回来: {s}"));
        assert_eq!(back.as_str(), code.as_str());
        assert!(host.is_some(), "{s} 应带地址");
    }
}

/// 进剪贴板的那一串必须**自带地址**，否则跨覆盖网配对必然卡壳。
///
/// 这是一条回归测试。此前 `host()` 复制的是裸的 6 位码，带地址那串只印在
/// 弹窗正文里等用户自己选中——于是 Tailscale 场景下对方粘过来只有码，退回
/// 局域网组播发现，而覆盖网不转发组播，最后还是得手敲 IP。整套"把地址并进
/// 配对码"的设计因为那一行而完全没生效，而且从任何单元测试里都看不出来。
#[test]
fn share_string_carries_an_address() {
    let code = PairingCode::parse("ABCDEF").unwrap();
    let s = share_string(&code, 47_684);

    // 无网卡的构建环境里退化成裸码，这时只要求它仍是个合法配对码。
    let (parsed, host) = parse_pairing_input(&s).expect("复制出去的串必须能被解析回来");
    assert_eq!(parsed.as_str(), "ABCDEF");
    if best_addr_for_sharing(47_684).is_some() {
        assert!(host.is_some(), "本机有可分享地址时，串里必须带上它");
    }
}

/// 弹窗正文只给**一串**，且正是复制进剪贴板的那一串。
///
/// 分两串（裸码 + 带地址的）等于让用户自己判断"我们算不算同一个局域网"，
/// 判断错了就配不上——而这恰恰是程序该替他判断的事。
#[test]
fn dialog_body_shows_exactly_what_was_copied() {
    let share = "ABCDEF@100.88.88.22";
    let body = dialog_body(share, true);
    assert!(body.contains(share));
    assert_eq!(body.matches("ABCDEF").count(), 1, "正文里不该出现第二串码");
}
