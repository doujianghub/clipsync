//! `wiring` 的单元测试（单独成文件以控制行数，仍是其子模块）。

use super::*;

/// 只有**全部**路径都在落地目录下才算"我们自己刚写入的接收文件"。
///
/// 若只要有一个命中就跳过，用户把收到的文件和自己的文件一起复制时，
/// 这次真实的复制会被误当作回声而丢失。
#[test]
fn received_files_detection_requires_all_paths_inside() {
    use clipsync_clip::ClipRead;
    use clipsync_core::{ClipContent, FileMeta};

    let recv = std::path::PathBuf::from("/tmp/ClipSync/recv");
    let mk = |paths: Vec<&str>| ClipRead {
        content: ClipContent::Files(vec![FileMeta::new("a", 1, 1)]),
        sensitive: false,
        file_paths: paths.into_iter().map(std::path::PathBuf::from).collect(),
        denied: Vec::new(),
    };

    assert!(is_our_received_files(
        &mk(vec!["/tmp/ClipSync/recv/0001/a.txt"]),
        &recv
    ));
    assert!(!is_our_received_files(
        &mk(vec!["/tmp/ClipSync/recv/0001/a.txt", "/Users/me/b.txt"]),
        &recv
    ));
    assert!(!is_our_received_files(&mk(vec![]), &recv));
}
