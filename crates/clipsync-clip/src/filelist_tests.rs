//! `filelist` 的单元测试。
//!
//! 单独成文件只为让 `filelist.rs` 保持在项目约定的行数以内；
//! 它仍是 `filelist` 的子模块（由 `#[path]` 引入），`use super::*`
//! 照常可用，与写在原文件里没有区别。

use super::*;

#[test]
fn meta_reflects_name_and_size() {
    let dir = std::env::temp_dir().join("clipsync_meta_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hello.txt");
    std::fs::write(&path, b"hello world").unwrap();

    let meta = meta_for_path(&path).unwrap();
    assert_eq!(meta.name, "hello.txt");
    assert_eq!(meta.size, 11);
    assert_ne!(meta.id, 0);
}

/// 同一文件（未改动）两次取元数据应得到相同标识——这是缓存命中的前提。
#[test]
fn meta_id_is_stable_for_unchanged_file() {
    let dir = std::env::temp_dir().join("clipsync_meta_test2");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("stable.bin");
    std::fs::write(&path, vec![7u8; 64]).unwrap();

    let a = meta_for_path(&path).unwrap();
    let b = meta_for_path(&path).unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(a, b);
}

/// 内容改变（大小随之改变）应产生不同标识，使旧缓存失效。
#[test]
fn meta_id_changes_when_file_changes() {
    let dir = std::env::temp_dir().join("clipsync_meta_test3");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("changing.bin");

    std::fs::write(&path, b"first").unwrap();
    let a = meta_for_path(&path).unwrap();

    std::fs::write(&path, b"second-and-longer").unwrap();
    let b = meta_for_path(&path).unwrap();

    assert_ne!(a.id, b.id, "文件改动后标识必须变化，否则会用到脏缓存");
}

#[test]
fn missing_file_reports_error() {
    let path = std::env::temp_dir().join("clipsync_definitely_missing_12345.bin");
    assert!(meta_for_path(&path).is_err());
}

/// 经剪贴板读回的文件名必须与磁盘上的字节一致（NFC）。
///
/// `NSURL` 往返会把文件名规范化成 NFD（`é` 从单码位 U+00E9 拆成
/// `e` + U+0301）。若不归一化回 NFC，同一个文件在"直接读磁盘"与
/// "经剪贴板读取"两条路径下会算出不同的 `id`（`id` 由文件名派生），
/// 跨平台缓存命中与断点续传对含重音字符的文件就会**静默失效**——
/// 不报错，只表现为每次都重传。
///
/// 这个差异是 Windows 侧联调时发现的：Windows 源端 `café.txt` 是
/// `c3a9`(NFC)，从 Mac 收到的却是 `65cc81`(NFD)。
#[cfg(target_os = "macos")]
#[test]
fn clipboard_roundtrip_preserves_nfc_filename() {
    let _guard = crate::clipboard_test_lock();
    let dir = std::env::temp_dir().join("clipsync_nfc_test");
    std::fs::create_dir_all(&dir).unwrap();
    // 显式用 NFC 形式的 é（U+00E9 单码位）建文件。
    let path = dir.join("caf\u{e9}.txt");
    std::fs::write(&path, b"nfc").unwrap();

    let on_disk = meta_for_path(&path).unwrap();
    assert_eq!(
        on_disk.name, "caf\u{e9}.txt",
        "磁盘上的文件名应为 NFC（前置条件）"
    );

    write_file_paths(std::slice::from_ref(&path)).expect("写入剪贴板应成功");
    let read = read_file_paths()
        .expect("读取不应出错")
        .expect("应读到文件");
    let via_clipboard = meta_for_path(&read[0]).unwrap();

    assert_eq!(
        via_clipboard.name, on_disk.name,
        "经剪贴板读回的文件名被规范化成了 NFD，与磁盘不一致"
    );
    assert_eq!(
        via_clipboard.id, on_disk.id,
        "同一文件经两条路径算出不同 id——缓存命中与断点续传会失效"
    );
}

/// 多文件写入必须在**写入进程退出后**仍然完整。
///
/// macOS 上 `writeObjects` 向 pasteboard 服务的提交是异步的：若不回读强制
/// 完成往返，进程一退出就只有第一条能存活，多文件复制会静默只剩一个文件。
/// 关键是必须跨进程验证——同一进程内即便没有回读也能看到全部条目，
/// 只有另一个进程（真实的粘贴方）才会看到被截断的结果。
///
/// 用 `write_files` 示例作为"写入后立即退出"的子进程。示例不存在时跳过。
#[cfg(target_os = "macos")]
#[test]
fn written_files_survive_writer_exit() {
    let _guard = crate::clipboard_test_lock();
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };
    // 测试二进制在 target/<profile>/deps/ 下，示例在 target/<profile>/examples/。
    let helper = exe
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("examples").join("write_files"));
    let Some(helper) = helper.filter(|p| p.exists()) else {
        eprintln!("跳过：未找到 write_files 示例（先跑 cargo build --examples）");
        return;
    };

    let dir = std::env::temp_dir().join("clipsync_filelist_exit");
    std::fs::create_dir_all(&dir).unwrap();
    let paths: Vec<_> = (0..4)
        .map(|i| {
            let p = dir.join(format!("e{i}.txt"));
            std::fs::write(&p, format!("file {i}")).unwrap();
            p
        })
        .collect();

    let status = std::process::Command::new(&helper)
        .args(&paths)
        .status()
        .expect("运行 write_files 示例失败");
    assert!(status.success(), "写入子进程应成功退出");

    // 子进程已退出，此时读到的就是真实粘贴方会看到的内容。
    let read = read_file_paths()
        .expect("读取不应出错")
        .expect("写入进程退出后仍应读到文件列表");
    assert_eq!(
        read.len(),
        paths.len(),
        "写入进程退出后条目被截断——writeObjects 的异步提交未被强制完成"
    );
    for p in &paths {
        assert!(read.contains(p), "缺少 {}", p.display());
    }
}

/// 权限被拒必须能从 `anyhow` 的因果链里认出来。
///
/// 上层给错误加过 context，`anyhow!("...{e}")` 那种写法会把 `io::Error` 格式化
/// 成字符串、丢掉类型，调用方就再也分不出"系统不让读"和"文件不存在"——而这
/// 两者对用户的意义完全不同：一个要去点系统设置，一个只是文件没了。
#[test]
fn permission_denied_survives_the_context_chain() {
    use anyhow::Context;

    let denied: anyhow::Error =
        Err::<(), _>(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .context("读取文件信息失败 /some/path")
            .context("再套一层")
            .unwrap_err();
    assert!(super::is_permission_denied(&denied));

    let missing: anyhow::Error = Err::<(), _>(std::io::Error::from(std::io::ErrorKind::NotFound))
        .context("读取文件信息失败 /some/path")
        .unwrap_err();
    assert!(
        !super::is_permission_denied(&missing),
        "文件不存在不是权限问题，不该引导用户去改系统设置"
    );

    assert!(!super::is_permission_denied(&anyhow::anyhow!("随便什么错")));
}

/// 被拒时的解释按**路径形态**分类，不看 errno。
///
/// macOS 的 TCC 一律返回 `Operation not permitted`，错误码里没有任何线索说明
/// 是哪一类保护——而"去哪一页开开关"完全取决于是哪一类。
#[test]
fn denial_is_explained_by_where_the_file_lives() {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return; // 无 HOME 的环境（容器）跳过
    }
    let at = |sub: &str| super::explain_denied(std::path::Path::new(&format!("{home}/{sub}")));

    // 另一个 App 的容器——微信、QQ 这类把收到的文件存在自己地盘里。
    let wechat = at("Library/Containers/com.tencent.xinWeChat/Data/tmp/报告.pdf");
    assert!(wechat.reason.contains("App"), "{}", wechat.reason);
    assert!(
        wechat.where_to_fix.contains("文件与文件夹"),
        "{}",
        wechat.where_to_fix
    );

    // 受保护的个人文件夹，三个分别指向同一页但理由各不相同。
    for (sub, want) in [
        ("Desktop/a.txt", "桌面"),
        ("Documents/a.txt", "文稿"),
        ("Downloads/a.txt", "下载"),
    ] {
        let d = at(sub);
        assert!(d.reason.contains(want), "{sub} → {}", d.reason);
        assert!(d.where_to_fix.contains("文件与文件夹"));
    }

    // 外置/网络卷是另一页。
    let vol = super::explain_denied(std::path::Path::new("/Volumes/U盘/a.txt"));
    assert!(vol.where_to_fix.contains("卷宗"), "{}", vol.where_to_fix);

    // 认不出来的位置也要给一条出路，不能只说"失败了"。
    let unknown = super::explain_denied(std::path::Path::new("/opt/whatever/a.txt"));
    assert!(!unknown.where_to_fix.is_empty());
}

/// **stat 过了不等于读得了**——只测元数据会漏掉最难查的那一类失败。
///
/// 回归自实机故障：从微信复制文件，本机 `stat` 成功、照常广播元数据、日志写着
/// "已同步"；对端索取内容时才发现打不开，默默丢弃，**它的剪贴板原封不动**——
/// 用户在那边粘出来的是上一次复制的东西。两边都没提示，现象是"同步串台了"。
///
/// `/etc/sudoers` 是现成的样本：0440 root:wheel，普通用户 stat 得到 1709 字节，
/// open 直接 PermissionDenied。
#[test]
#[cfg(unix)]
fn metadata_alone_is_not_enough_to_call_a_file_readable() {
    let path = std::path::Path::new("/etc/sudoers");
    if std::fs::metadata(path).is_err() || std::fs::File::open(path).is_ok() {
        return; // 不是这台机器上的样本（或以 root 在跑），跳过
    }
    // 前提确认：stat 是通的，所以只看 stat 一定会误判为"可同步"。
    assert!(std::fs::metadata(path).is_ok(), "样本前提：stat 应当成功");

    let e = super::meta_for_path(path).expect_err("读不了的文件不该被当成可同步");
    assert!(
        super::is_permission_denied(&e),
        "要能认出是权限问题，才好引导用户去开开关：{e:#}"
    );
}
