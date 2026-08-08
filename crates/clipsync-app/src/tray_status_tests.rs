//! `tray_status` 的单元测试。
//!
//! 单独成文件只为让 `tray_status.rs` 保持在项目约定的行数以内；它仍是
//! `tray_status` 的子模块（由 `#[path]` 引入），`use super::*` 照常可用。

use super::*;

fn prog(name: &str, done: u64, total: u64) -> TransferProgress {
    TransferProgress {
        sending: false,
        name: name.into(),
        done,
        total,
    }
}

/// 分母跟着设备表走，**任何**改表的路径都算数。
///
/// 回归自实机反馈「已连接 2 / 1 台」。原先托盘自己存着一份台数，由配对
/// 流程手工同步；而改表的地方有四处，引荐认识一台新设备和对端广播移出
/// 都不经过配对流程。分子来自连接、分母停在旧值，于是分子大过分母。
///
/// 这里刻意**只动计数句柄、不碰托盘**——模拟的正是那两条"绕过托盘"的
/// 路径。托盘还得手工同步的话，这条就过不了。
#[test]
fn paired_count_follows_the_device_table() {
    let table = Arc::new(AtomicUsize::new(1));
    let s = TrayStatus::tracking(table.clone());

    s.set_connected_ids(["a".into()].into_iter().collect());
    assert!(s.summary().contains("已连接 1 / 1"), "{}", s.summary());

    // 经引荐认识了第二台并连上。托盘那份分母若还要人手工同步，这里就是
    // 实机上看到的「已连接 2 / 1 台」。
    table.store(2, Ordering::Release);
    s.set_connected_ids(["a".into(), "b".into()].into_iter().collect());
    assert!(s.summary().contains("已连接 2 / 2"), "{}", s.summary());

    // 对端广播把 b 移出，中枢随即断开它。
    table.store(1, Ordering::Release);
    s.set_connected_ids(["a".into()].into_iter().collect());
    assert!(s.summary().contains("已连接 1 / 1"), "{}", s.summary());
}

/// 传输中，摘要要报进度而不是"已连接 N 台"——这时用户最想知道的是到哪了。
#[test]
fn summary_reports_progress_while_transferring() {
    let s = TrayStatus::new(2);
    s.set_connected_ids(["a".to_string()].into_iter().collect());
    assert!(s.summary().contains("已连接"));

    s.note_transfer(prog("video.mp4", 42, 100));
    let sum = s.summary();
    assert!(sum.contains("接收"), "要说方向，实际 {sum}");
    assert!(sum.contains("video.mp4"), "要说文件名，实际 {sum}");
    assert!(sum.contains("42%"), "要说百分比，实际 {sum}");
}

/// 过期判定的边界。
#[test]
fn transfer_recency_boundaries() {
    let w = TRANSFER_LINGER.as_millis() as u64;
    assert!(!transfer_is_recent(0, 0), "从未传输过");
    assert!(!transfer_is_recent(9_999, 0), "从未传输过，多久都不算");
    assert!(transfer_is_recent(100, 100), "刚刚就是现在");
    assert!(transfer_is_recent(100 + w - 1, 100), "窗口内");
    assert!(!transfer_is_recent(100 + w, 100), "刚好到窗口边界即过期");
    // 时钟不会倒流，但真倒了也不能 panic 或误判成"很久以前"。
    assert!(transfer_is_recent(50, 100), "负差被 saturating 夹到 0");
}

/// 进度必须**自己过期**，不能依赖各处出口记得清。
///
/// 传输的结束路径有一大把——传完、被新内容取代、对端 FileAbort、解压
/// 失败、写盘失败、连接断开——挨个补一句清理迟早会漏，漏了就是托盘上
/// 永远挂着一条"接收 42%"。
///
/// 这条测试要真等一个 `TRANSFER_LINGER`（1.5 秒）。值：它验的是"摘要
/// 确实挂在 `is_transferring` 上"这个联动，而联动正是漏清理时唯一还能
/// 兜住的东西；边界算术已由上一条免费覆盖。
#[test]
fn progress_expires_without_anyone_clearing_it() {
    let s = TrayStatus::new(1);
    s.note_transfer(prog("big.zip", 1, 100));
    assert!(s.summary().contains("big.zip"));

    std::thread::sleep(TRANSFER_LINGER + Duration::from_millis(50));
    let sum = s.summary();
    assert!(!sum.contains("big.zip"), "过期后不该还挂着进度：{sum}");
    assert!(sum.contains("未连接") || sum.contains("已连接") || sum.contains("尚未配对"));
}

/// 换文件要重新锚定速度，否则新文件的速率被上一个的平均值污染。
#[test]
fn switching_files_reanchors_the_rate() {
    let s = TrayStatus::new(1);
    s.note_transfer(prog("a.bin", 0, 1000));
    s.note_transfer(prog("a.bin", 900, 1000));

    s.note_transfer(prog("b.bin", 0, 1000));
    let g = s.progress.lock().unwrap();
    let st = g.as_ref().unwrap();
    assert_eq!(st.since_bytes, 0, "新文件的速度锚点应从它自己的起点算");
    assert_eq!(st.p.name, "b.bin");
}

/// 刚开头不报速度：由极短时间算出的数字往往离谱，不如不显示。
#[test]
fn rate_is_withheld_until_it_is_meaningful() {
    let s = TrayStatus::new(1);
    s.note_transfer(prog("x.bin", 500, 1000));
    let sum = s.summary();
    assert!(sum.contains("50%"));
    assert!(!sum.contains("/s"), "刚开头不该报速度：{sum}");
}

/// 单位相同就只写一次——重复的单位不带信息，白占五列。
#[test]
fn byte_pairs_drop_the_repeated_unit() {
    assert_eq!(bytes_pair(4 << 30, 9 << 30), "4 / 9 GiB");
    // 单位不同就得都写全，否则读者会误以为同一量级。
    let mixed = bytes_pair(151 << 20, 17 << 30);
    assert!(mixed.contains("MiB") && mixed.contains("GiB"), "{mixed}");
}

/// 短名字原样保留，不该无端加省略号。
#[test]
fn short_names_pass_through() {
    assert_eq!(ellipsize_middle("a.txt"), "a.txt");
    assert_eq!(ellipsize_middle("报告.pdf"), "报告.pdf");
    // 正好 5+1+5 也不截。
    assert_eq!(ellipsize_middle("abcdeXfghij"), "abcdeXfghij");
}

/// 超长时固定取头 5 尾 5，**扩展名必须留着**。
#[test]
fn long_names_keep_five_at_each_end() {
    let out = ellipsize_middle("2026年度第三季度产品发布会现场录像完整版第三部分.mp4");
    assert_eq!(out, "2026年…分.mp4");
    assert_eq!(out.chars().count(), 11);

    let iso = ellipsize_middle("Ubuntu-24.04.1-desktop-amd64-live-server-installer.iso");
    assert_eq!(iso, "Ubunt…r.iso");
}

/// 截断长度**不随剩余空间浮动**。
///
/// 回归自实机观感："有时候换行有时候不换行"。若按剩余预算分配头尾，
/// 速率位数一变（9.8 ↔ 123.4 MiB/s）名字长度就跟着变，总宽在折行临界点
/// 上下抖，提示一会儿一行一会儿两行。
#[test]
fn name_length_is_fixed_regardless_of_context() {
    let long = "IMG_20260807_143052_HDR_Portrait_Enhanced_Final_v3.heic";
    let a = ellipsize_middle(long);
    let b = ellipsize_middle(long);
    assert_eq!(a, b);
    assert_eq!(a.chars().count(), NAME_HEAD + 1 + NAME_TAIL);
}

/// 托盘提示**任何情况下**都不能超过 63 个字符。
///
/// 回归自实机截图：文案在第 64 个字符上被系统一刀切断，后半截连百分比
/// 都看不到。上一轮只截了文件名，可固定部分（`ClipSync — 接收 ` 加上
/// 绝对字节数与速率）本身就占掉五十来个，光截名字不够。
#[test]
fn tooltip_never_exceeds_the_windows_limit() {
    let cases: [(&str, u64, u64); 4] = [
        (
            "sha256-1194192cf2b8e4a09d7c3f5061e2a78863006a.tar.zst",
            159_000_000,
            18_500_000_000,
        ),
        (
            "这是一个特别特别特别特别长的中文文件名用来测试截断.mkv",
            1,
            100,
        ),
        ("a.txt", 50, 100),
        (&"x".repeat(300), 1, 2),
    ];
    for (name, done, total) in cases {
        let s = TrayStatus::new(3);
        s.set_connected_ids(["a".into(), "b".into()].into_iter().collect());
        s.note_transfer(TransferProgress {
            sending: false,
            name: name.into(),
            done,
            total,
        });
        // 走一遍会显示速率的分支：预算是倒推出来的，速率位数变化不该把
        // 总长顶出去。
        std::thread::sleep(std::time::Duration::from_millis(600));
        s.note_transfer(TransferProgress {
            sending: false,
            name: name.into(),
            done: done + 12_345_678,
            total,
        });

        let tip = s.tooltip();
        let n = tip.chars().count();
        assert!(n <= 63, "提示 {n} 字符，超了：{tip}");
        assert!(tip.contains('%'), "百分比不能被挤掉：{tip}");

        // 固定两行：自己换行才不会随宽度抖。
        let lines: Vec<&str> = tip.split('\n').collect();
        assert_eq!(lines.len(), 2, "传输中的提示应恒为两行：{tip:?}");
        assert!(lines[0].contains('%'), "第一行给方向、名字、进度：{tip:?}");
        assert!(lines[1].contains('/'), "第二行给已传/总量：{tip:?}");
        // 任一行都不该长到被系统再折一次。
        for l in &lines {
            assert!(l.chars().count() <= 40, "行太长会被再折一次：{l}");
        }
    }
}

/// 没有传输时提示也得守住上限（设备名可以很长）。
#[test]
fn idle_tooltip_also_fits() {
    let s = TrayStatus::new(9);
    assert!(s.tooltip().chars().count() <= 63);
    s.set_connected_ids((0..9).map(|i| i.to_string()).collect());
    assert!(s.tooltip().chars().count() <= 63);
}

/// 菜单与提示都要给出绝对字节数——只看百分比不知道还剩多少。
#[test]
fn menu_summary_keeps_the_absolute_bytes() {
    let s = TrayStatus::new(1);
    s.note_transfer(TransferProgress {
        sending: false,
        name: "video.mkv".into(),
        done: 0,
        total: 17_300_000_000,
    });
    std::thread::sleep(std::time::Duration::from_millis(600));
    s.note_transfer(TransferProgress {
        sending: false,
        name: "video.mkv".into(),
        done: 151_800_000,
        total: 17_300_000_000,
    });
    let sum = s.summary();
    assert!(
        sum.contains("GiB") || sum.contains("MiB"),
        "菜单里该有字节数：{sum}"
    );
    // 提示里也要有已传/总量——只看百分比不知道还剩多少。
    let tip = s.tooltip();
    assert!(tip.contains(" / "), "提示第二行该给出已传/总量：{tip}");
}

/// 人工核对截断效果。
#[test]
#[ignore = "只为肉眼看效果"]
fn manual_show_truncation() {
    for name in [
        "report.pdf",
        "2026年度第三季度产品发布会现场录像完整版第三部分.mp4",
        "Ubuntu-24.04.1-desktop-amd64-live-server-installer.iso",
        "会议纪要.docx",
        "IMG_20260807_143052_HDR_Portrait_Enhanced_Final_v3.heic",
        "备份-王信的Mac mini-2026-08-07-完整系统镜像.dmg",
    ] {
        let s = TrayStatus::new(1);
        s.note_transfer(TransferProgress {
            sending: false,
            name: name.into(),
            done: 4_200_000_000,
            total: 10_000_000_000,
        });
        // 走到会显示速率的分支——那才是传输中的常态，文案也最长。
        std::thread::sleep(std::time::Duration::from_millis(600));
        s.note_transfer(TransferProgress {
            sending: false,
            name: name.into(),
            done: 4_230_000_000,
            total: 10_000_000_000,
        });
        println!("原名 {name}");
        println!("  菜单 {}", s.summary());
        let tip = s.tooltip();
        for (i, l) in tip.split('\n').enumerate() {
            println!("  提示{} {l}", i + 1);
        }
        println!("       [共 {} 字符]", tip.chars().count());
    }
}

/// 空闲时的提示要带上待取项——不打开菜单也知道有东西等着。
#[test]
fn idle_tooltip_mentions_pending_files() {
    let s = TrayStatus::new(2);
    s.set_connected_ids(["a".into(), "b".into()].into_iter().collect());
    assert!(!s.tooltip().contains("待取"), "没东西待取时别无端加一行");

    s.set_pending(Some(PendingFetchInfo {
        from: "a".into(),
        first_name: "备份.dmg".into(),
        count: 3,
        total: 4_500_000_000,
    }));
    let tip = s.tooltip();
    assert!(tip.contains("3 项待取回"), "该说有几项：{tip}");
    assert!(tip.contains("GiB"), "该说多大：{tip}");
    assert!(
        tip.chars().count() <= 63,
        "仍不能超过 Windows 的硬上限：{tip}"
    );
}

/// 传输中不提待取项——那两行已经把 63 个字符占满了。
///
/// 眼下正在动的那件事更值得看；传完自然会退回空闲态并带上待取那一行。
#[test]
fn transfer_tooltip_stays_two_lines_even_with_pending() {
    let s = TrayStatus::new(1);
    s.set_pending(Some(PendingFetchInfo {
        from: "a".into(),
        first_name: "另一个大文件.zip".into(),
        count: 1,
        total: 9_000_000_000,
    }));
    s.note_transfer(TransferProgress {
        sending: false,
        name: "正在传的.mp4".into(),
        done: 42,
        total: 100,
    });
    let tip = s.tooltip();
    assert_eq!(tip.split('\n').count(), 2, "传输中恒为两行：{tip:?}");
    assert!(!tip.contains("待取"), "别把进度挤掉：{tip}");
    assert!(tip.chars().count() <= 63);
}

/// 状态行在英文界面下必须是英文。
///
/// 托盘提示是最常被看到的一行字——它要是漏译了，英文用户每次瞄一眼托盘都会
/// 撞见中文，而开发者在中文系统上永远看不到这个问题。
#[test]
fn the_status_line_is_translated() {
    use clipsync_core::Lang;

    let status = TrayStatus::new(2);
    status.set_connected_ids(std::collections::HashSet::from(["a".to_string()]));

    let (zh, en) = (
        crate::language::with_lang(Lang::Zh, || status.summary()),
        crate::language::with_lang(Lang::English, || status.summary()),
    );
    assert_eq!(zh, "ClipSync — 已连接 1 / 2 台");
    assert_eq!(en, "ClipSync — 1 / 2 connected");

    // 暂停态同理——它是另一条独立分支，漏译过一次就会一直漏。
    status.set_paused(true);
    assert_eq!(
        crate::language::with_lang(Lang::English, || status.summary()),
        "ClipSync — paused"
    );
}
