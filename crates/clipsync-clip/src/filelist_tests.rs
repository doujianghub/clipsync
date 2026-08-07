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
