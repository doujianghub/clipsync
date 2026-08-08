//! `arboard_backend` 的单元测试。
//!
//! 单独成文件只为让 `arboard_backend.rs` 保持在项目约定的行数以内；
//! 它仍是 `arboard_backend` 的子模块（由 `#[path]` 引入），`use super::*`
//! 照常可用，与写在原文件里没有区别。

use super::*;

/// 端到端回归：arboard 解不了的图片，整条读取链路也必须能拿到。
///
/// 与 `image_mac` 里那条的区别在于测的是**用户可见行为**：那条只证明兜底
/// 函数本身能工作，这条证明 `read()` 真的会在 arboard 失败时走到兜底——
/// 接错线的话那条照样绿，用户的图片照样同步不过去。
///
/// 场景是 Retina 屏上经 `NSImage.lockFocus` 绘制再复制的图片，格式为
/// 16-bit 浮点 TIFF，arboard 内部的 `image` 0.25 返回 `ConversionFailure`。
///
/// > 验证这条时踩过一个坑，值得记下来：曾用一个独立小程序直接调
/// > `arboard::get_image()` 做对照，得到"能解"的结论，差点据此判定缺口
/// > 已消失并删掉整个兜底。实际原因是当时**后台还跑着两个 clipsync 实例**
/// > ——它们把图片同步一圈后用 arboard 写回了剪贴板，对照程序读到的是
/// > 已被转成 8-bit 的版本。做剪贴板对照实验前务必确认没有本程序在跑。
#[cfg(target_os = "macos")]
#[test]
fn float_tiff_image_is_readable_end_to_end() {
    let _guard = crate::clipboard_test_lock();

    let script = r#"import AppKit
let img = NSImage(size: NSSize(width: 40, height: 30))
img.lockFocus()
NSColor(srgbRed: 0.0, green: 1.0, blue: 0.0, alpha: 1.0).setFill()
NSRect(x: 0, y: 0, width: 40, height: 30).fill()
img.unlockFocus()
NSPasteboard.general.clearContents()
NSPasteboard.general.writeObjects([img])
"#;
    let spawned = std::process::Command::new("swift")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.take().unwrap().write_all(script.as_bytes())?;
            c.wait()
        });
    match spawned {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("跳过：swift 不可用");
            return;
        }
    }

    // 前置条件：确认剪贴板里**真是**浮点 TIFF。少了这一步，本用例可能只是
    // 在验证一张普通 8-bit 图，看着绿油油却守不住任何东西。
    {
        use objc2_app_kit::{NSBitmapFormat, NSBitmapImageRep, NSPasteboard, NSPasteboardTypeTIFF};
        let pb = NSPasteboard::generalPasteboard();
        // SAFETY: AppKit 导出的常量类型标识。
        let Some(raw) = (unsafe { pb.dataForType(NSPasteboardTypeTIFF) }) else {
            eprintln!("跳过：剪贴板里没有 TIFF");
            return;
        };
        let rep = NSBitmapImageRep::imageRepWithData(&raw).expect("AppKit 应能解析自己写的 TIFF");
        if !rep
            .bitmapFormat()
            .contains(NSBitmapFormat::FloatingPointSamples)
        {
            eprintln!(
                "跳过：本机产出的是 {} bps 非浮点图，构造不出目标场景",
                rep.bitsPerSample()
            );
            return;
        }
    }

    let Ok(mut cb) = ArboardClipboard::new() else {
        eprintln!("跳过：无可用剪贴板");
        return;
    };
    let read = cb
        .read()
        .expect("读取不应报错——arboard 解不了时应由平台路径兜底")
        .expect("应读到内容");

    match read.content {
        ClipContent::Image(img) => {
            assert!(img.width > 0 && img.height > 0);
            assert_eq!(img.rgba.len(), (img.width * img.height * 4) as usize);
            // 画的是纯绿；顺带守住兜底路径没把通道顺序搞反。
            let px = &img.rgba[..4];
            assert!(px[1] > 200, "绿通道应接近满值，实际 {px:?}");
            assert!(px[0] < 60 && px[2] < 60, "红/蓝通道应接近 0，实际 {px:?}");
        }
        other => panic!("应读到图片，实际是 {other:?}——兜底未接上，图片会同步失败"),
    }
}

/// 回归：元数据与路径必须严格一一对应，即便中途有文件读不到。
///
/// 触发条件是**不同目录下的同名文件**：旧实现先收集元数据，再按
/// `file_name()` 反查该保留哪些路径。两条路径的文件名相同，只要其中
/// 一个取元数据失败，反查就会把两条都留下——于是 1 条元数据配 2 条路径。
/// 上层 `hub` 用 `zip` 配对，静默取前 1 组，结果是拿着 B 文件的标识
/// （大小/哈希）去发送 A 文件的字节，对端校验失败。
///
/// 需要真实剪贴板，故限定 macOS；无 GUI 会话时自动跳过。
#[cfg(target_os = "macos")]
#[test]
fn same_named_files_keep_meta_and_path_aligned() {
    let _guard = crate::clipboard_test_lock();
    let root = std::env::temp_dir().join("clipsync_align_test");
    let _ = std::fs::remove_dir_all(&root);
    let (da, db) = (root.join("a"), root.join("b"));
    std::fs::create_dir_all(&da).unwrap();
    std::fs::create_dir_all(&db).unwrap();

    // 同名、不同目录、不同大小——大小差异让错配一眼可辨。
    let (pa, pb) = (da.join("x.txt"), db.join("x.txt"));
    std::fs::write(&pa, b"a").unwrap();
    std::fs::write(&pb, b"bb").unwrap();

    if !crate::clipboard_usable() {
        eprintln!("跳过：剪贴板不可用（无 GUI 会话）");
        return;
    }
    crate::filelist::write_file_paths(&[pa.clone(), pb.clone()]).unwrap();

    // 让第一个文件在"复制之后、读取之前"消失——这正是取元数据会失败的
    // 真实情形（用户复制后删掉了文件）。
    std::fs::remove_file(&pa).unwrap();

    let Ok(mut cb) = ArboardClipboard::new() else {
        eprintln!("跳过：无可用剪贴板（无 GUI 会话）");
        return;
    };
    let read = cb.read().expect("读取不应出错").expect("应读到文件");

    let ClipContent::Files(metas) = &read.content else {
        panic!("应读到文件列表，实际 {:?}", read.content);
    };
    assert_eq!(
        metas.len(),
        read.file_paths.len(),
        "元数据 {} 条、路径 {} 条——两者错位后上层 zip 会静默配错文件",
        metas.len(),
        read.file_paths.len()
    );
    assert_eq!(metas.len(), 1, "只有一个文件仍存在");
    assert_eq!(read.file_paths[0], pb, "保留的应是仍存在的那条路径");
    assert_eq!(
        metas[0].size, 2,
        "元数据应描述 b/x.txt（2 字节）而非已删除的 a/x.txt"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Windows 的 1418 必须被认成"占用"。
///
/// 这是回归测试：实机日志里出现过
/// `SetClipboardData failed with error: 线程没有打开的剪贴板。(os error 1418)`，
/// 而当时的判定只认 `ClipboardOccupied`，于是一次都没重试就报错了，用户那边
/// 表现为"复制了但对端没更新"。
#[test]
fn windows_contention_errors_are_recognised() {
    let unknown = |d: &str| arboard::Error::Unknown {
        description: d.into(),
    };

    assert!(is_occupied(&arboard::Error::ClipboardOccupied));
    assert!(is_occupied(&unknown(
        "SetClipboardData failed with error: 线程没有打开的剪贴板。 (os error 1418)"
    )));
    assert!(is_occupied(&unknown("OpenClipboard failed (os error 5)")));
}

/// 别把无关错误也当成占用——那会让真正的失败白白拖满整个重试预算。
#[test]
fn unrelated_errors_are_not_treated_as_contention() {
    let unknown = |d: &str| arboard::Error::Unknown {
        description: d.into(),
    };

    assert!(!is_occupied(&arboard::Error::ContentNotAvailable));
    assert!(!is_occupied(&arboard::Error::ConversionFailure));
    assert!(!is_occupied(&unknown("something else entirely")));
    // 右括号是防这个的：55 不是争用错误码，不能被 "(os error 5" 前缀钓上。
    assert!(!is_occupied(&unknown("failed (os error 55)")));
}

/// 占用类错误要真的重试，而且最终成功。
#[test]
fn an_occupied_clipboard_is_retried_until_it_frees_up() {
    let mut left = 3;
    let got = retry_on_occupied(|| {
        if left > 0 {
            left -= 1;
            Err(arboard::Error::Unknown {
                description: "SetClipboardData failed (os error 1418)".into(),
            })
        } else {
            Ok("写进去了")
        }
    });
    assert_eq!(got.unwrap(), "写进去了");
    assert_eq!(left, 0, "应当把三次占用都重试掉");
}

/// 非占用类错误必须立刻返回，一次都不重试。
#[test]
fn other_errors_fail_fast() {
    let mut calls = 0;
    let got: std::result::Result<(), _> = retry_on_occupied(|| {
        calls += 1;
        Err(arboard::Error::ConversionFailure)
    });
    assert!(got.is_err());
    assert_eq!(calls, 1, "无关错误重试没有意义，只会拖延报错");
}

/// 重试有总时间上限，不能无限堵住中枢线程。
///
/// 用一个"每次都很慢"的操作模拟写大图（实测 240ms/次），验证它不会像固定
/// 次数那样把预算翻好几倍。
#[test]
fn a_slow_operation_does_not_blow_the_budget() {
    let started = std::time::Instant::now();
    let got: std::result::Result<(), _> = retry_on_occupied(|| {
        std::thread::sleep(Duration::from_millis(200));
        Err(arboard::Error::ClipboardOccupied)
    });
    let took = started.elapsed();

    assert!(got.is_err());
    // 600ms 预算下每次 200ms，正常是 3 次约 800ms。上限取 1800ms：容得下慢
    // runner，又明显小于"固定 12 次重试"的 2400ms——退化了一眼就能看出来。
    assert!(
        took < Duration::from_millis(1800),
        "慢操作也该在预算附近收手，实际 {took:?}"
    );
}
