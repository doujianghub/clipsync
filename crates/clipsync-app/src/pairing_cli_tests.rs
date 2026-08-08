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
    let never = AtomicBool::new(false);
    let got = accept_until(&listener, deadline, &never).expect("轮询不应报错");
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
    let never = AtomicBool::new(false);
    let (stream, _) = accept_until(&listener, deadline, &never)
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
/// 这里刻意复用真实的 `dialog_body` 与 `show_info`，而不是另写
/// 一段相似的内容——照抄一遍只能证明抄得对，证明不了线上那条路径对。
/// 唯一省略的是 `TcpListener` 与 `accept` 循环：它们与"窗口显示成什么样"
/// 无关，却会让测试永久阻塞。
///
/// 跑法：`cargo test -p clipsync-app --bin clipsync -- --ignored pairing_dialog`
///
/// 判据：
///   1. 窗口标题为「ClipSync 配对」，正文首行的 4 位配对码清晰可读；
///   2. 有两个按钮，**默认落在「好」**上——换码是岔路，敲回车不该把码换掉；
///   3. 「有效至」那个时刻等于现在 + 3 分钟；
///   4. 点「换个配对码」打印 `ACTION=true`，点「好」/Esc 打印 `ACTION=false`。
#[test]
#[ignore = "会弹窗并阻塞，需人工/脚本关闭"]
fn manual_pairing_dialog() {
    let code = PairingCode::generate();
    // 打到 stdout，供外部脚本比对窗口里显示的是不是同一个码。
    println!("EXPECT_CODE={code}");
    let body = code_dialog_body(code.as_str(), 47_684, HOST_SESSION_TIMEOUT);
    println!("{body}");
    let acted = crate::dialog::ask_action("ClipSync 配对", &body, "换个配对码");
    println!("ACTION={acted}");
}

/// 纯配对码：不带地址，调用方走局域网自动发现。
#[test]
fn parses_bare_code() {
    let (code, host) = parse_pairing_input("1234").expect("应识别为配对码");
    assert_eq!(code.as_str(), "1234");
    assert!(host.is_none(), "没带地址就该让调用方去自动发现");
}

/// 带地址的串：跨网络配对的正路。
///
/// Tailscale 这类覆盖网不转发组播，自动发现注定落空——地址必须随码带过来，
/// 否则用户得先看一屏 IP 再手敲。
#[test]
fn parses_code_with_address() {
    let (code, host) = parse_pairing_input("1234@100.88.88.22").unwrap();
    assert_eq!(code.as_str(), "1234");
    assert_eq!(host.as_deref(), Some("100.88.88.22"));
}

/// IPv6 要从右往左切：地址里全是冒号，但不会有 @。
#[test]
fn parses_ipv6_address() {
    let (code, host) = parse_pairing_input("1234@[fd7a:115c:a1e0::e201:c839]").unwrap();
    assert_eq!(code.as_str(), "1234");
    assert_eq!(host.as_deref(), Some("[fd7a:115c:a1e0::e201:c839]"));
}

/// 用户输入不会规整：大小写、空格、连字符都得认（沿用 PairingCode::parse）。
#[test]
fn tolerates_messy_input() {
    assert_eq!(parse_pairing_input("  1234  ").unwrap().0.as_str(), "1234");
    assert_eq!(parse_pairing_input("12-34").unwrap().0.as_str(), "1234");
    let (c, h) = parse_pairing_input(" 1234@10.0.0.5 ").unwrap();
    assert_eq!((c.as_str(), h.as_deref()), ("1234", Some("10.0.0.5")));
}

#[test]
fn rejects_malformed_input() {
    assert!(parse_pairing_input("").is_none());
    assert!(parse_pairing_input("1234567").is_none());
    assert!(parse_pairing_input("1234@").is_none(), "@ 后面空着不算有效地址");
    // 字符集只有数字：码要靠人念、人敲，字母大小写与 O/0、l/1 之类的混淆
    // 在电话里说不清楚。
    assert!(parse_pairing_input("12A4").is_none());
}

/// `码@地址` 这个手动出口必须能被解析回来。
///
/// 它不再主动示人（自动发现覆盖了绝大多数情况），但仍是自动发现全落空时
/// 唯一不用重来一遍的退路，命令行也照旧接受。
#[test]
fn manual_code_with_address_round_trips() {
    let code = PairingCode::from_entropy(b"\x01\x02\x03\x04");
    for host in ["192.168.1.5", "[fd7a:115c:a1e0::1]", "100.88.88.22"] {
        let s = format!("{code}@{host}");
        let (back, parsed) = parse_pairing_input(&s)
            .unwrap_or_else(|| panic!("手动写法应能解析: {s}"));
        assert_eq!(back.as_str(), code.as_str());
        assert_eq!(parsed.as_deref(), Some(host));
    }
}

/// 弹窗正文要把码、去哪儿输、多久过期这三件事说全，且不带任何"复制"字样。
///
/// 曾经把码自动放进主持方剪贴板、让用户"复制粘贴过去"——这是循环依赖：
/// 本工具要解决的正是"跨设备复制粘贴还没打通"。这条测试把那条弯路钉死。
#[test]
fn dialog_body_tells_the_user_everything_and_mentions_no_clipboard() {
    let code = PairingCode::from_entropy(b"\x01\x02\x03\x04");
    let body = code_dialog_body(code.as_str(), 47_684, HOST_SESSION_TIMEOUT);

    assert!(body.contains(code.as_str()), "得有码");
    assert!(body.contains("输入配对码"), "得说去哪儿输");
    assert!(body.contains("配对成功即失效"), "得说清一个码只配一台");
    // 有效期写成绝对时刻：这个窗口显示出来就改不了字了，写「还有 2 分 47 秒」
    // 从那一刻起就在撒谎。取不到系统时钟才退回说总时长。
    match crate::wallclock::hms_after(HOST_SESSION_TIMEOUT) {
        Some(at) => assert!(body.contains(&format!("有效至 {at}")), "得说到几点：{body}"),
        None => assert!(body.contains("分钟内有效"), "退化路径也得说多久过期"),
    }
    assert!(!body.contains("剪贴板"), "不该再提剪贴板");
    assert!(!body.contains("复制"), "不该再提复制");
}

/// 人工核对两处地址列表的实际样子。
#[test]
#[ignore = "结果取决于本机网卡，只为肉眼看"]
fn manual_show_address_blocks() {
    println!("—— 配对码窗口 ——");
    println!("{}", code_dialog_body("1234", 47_684, HOST_SESSION_TIMEOUT));
    println!();
    println!("—— 手输地址时 ——");
    println!("请输入对方的 IP（对方窗口里有）：");
    if let Some(b) = addr_block(47_684) {
        println!();
        println!("本机地址，供对照挑同网段的：");
        println!("{b}");
    }
}

/// 地址列表要列**全部** IPv4 并标注类别，且同网段的排最前。
///
/// 回归自实机反馈：原先只挑"最合适"的一个（优先覆盖网），碰上对方只在
/// 局域网里，那个地址就是死的，用户还以为程序给错了。哪个地址通取决于
/// **对方**在哪张网上，本机无从知道——列全了由人来挑。
#[test]
fn address_block_lists_every_ipv4_lan_first() {
    let lines = local_ipv4_lines(47_684);
    // 构建环境可能一张网卡都没有，那时为空也是对的。
    if lines.is_empty() {
        return;
    }
    for l in &lines {
        assert!(
            l.contains("（局域网）") || l.contains("（覆盖网）") || l.contains("（公网）"),
            "每条都要标类别：{l}"
        );
        assert!(!l.contains(':'), "只列 IPv4，不该出现 IPv6 或端口：{l}");
    }
    // TUN 模式代理的假 IP 段永不可路由，列出来只会让人挑错。
    assert!(
        !lines.iter().any(|l| l.starts_with("198.18.") || l.starts_with("198.19.")),
        "RFC 2544 基准测试段不该出现：{lines:?}"
    );

    // 局域网的必须排在覆盖网/公网之前，与连接时的优选顺序一致。
    let first_non_lan = lines.iter().position(|l| !l.contains("（局域网）"));
    if let Some(i) = first_non_lan {
        assert!(
            lines[i..].iter().all(|l| !l.contains("（局域网）")),
            "局域网地址应连续排在最前：{lines:?}"
        );
    }
}

/// 「换个配对码」必须**当场**生效，而不是等到会话到期。
///
/// 取消标志是靠 accept 轮询发现的（`accept()` 一旦阻塞就叫不醒）。这条把
/// deadline 设在很远的地方，能跑完就说明走的是取消这条路。
#[test]
fn accept_until_gives_up_promptly_when_cancelled() {
    use std::sync::Arc;

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();

    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        flag.store(true, Ordering::Release);
    });

    let started = Instant::now();
    let got = accept_until(&listener, Instant::now() + Duration::from_secs(600), &cancel)
        .expect("取消不是错误");
    assert!(got.is_none(), "被取消时不该报告有连接");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "取消应在一两个轮询周期内生效，实际等了 {:?}",
        started.elapsed()
    );
}

/// `Cancelled` 必须能从 `anyhow::Error` 里认出来。
///
/// 调用方据此区分"用户要换码"（接着开新一轮，不弹窗）和"真出事了"（弹一个
/// 配对失败）。若改成靠错误文案匹配，改一次措辞就会让用户在换码时看到一个
/// 莫名其妙的失败框——`WrongCode` 也是同样的道理。
#[test]
fn cancelled_is_recognisable_through_anyhow() {
    let e = anyhow::Error::new(Cancelled);
    assert!(e.downcast_ref::<Cancelled>().is_some());
    // 别的错误不能被误认成取消，否则真故障会被当成换码悄悄吞掉。
    let other = anyhow::anyhow!("监听配对端口 47685 失败");
    assert!(other.downcast_ref::<Cancelled>().is_none());
}
