//! `filecache` 的单元测试。
//!
//! 单独成文件只为让 `filecache.rs` 保持在项目约定的行数以内；
//! 它仍是 `filecache` 的子模块（由 `#[path]` 引入），`use super::*`
//! 照常可用，与写在原文件里没有区别。

use super::*;

fn temp_cache(tag: &str, limit: u64) -> FileCache {
    // 用独立子目录避免测试相互干扰。
    let root = std::env::temp_dir().join(format!("ClipSyncTest_{tag}"));
    let _ = std::fs::remove_dir_all(&root);
    for sub in ["parts", "blobs", "recv"] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
    }
    FileCache { root, limit }
}

#[test]
fn append_and_finalize_roundtrip() {
    let c = temp_cache("roundtrip", 1 << 30);
    let data = b"hello clipsync file transfer";
    let id = 0xABCDEF;

    assert_eq!(c.have_bytes(id, data.len() as u64), 0);
    let n = c.append(id, 0, data).unwrap();
    assert_eq!(n, data.len() as u64);

    let expected = {
        let mut h = clipsync_core::hash::Hasher::new();
        h.update(data);
        h.finish()
    };
    c.finalize(id, expected).unwrap();
    assert!(c.is_complete(id, data.len() as u64));
    // 完整命中后，续传起点等于全长——意味着无需任何传输。
    assert_eq!(c.have_bytes(id, data.len() as u64), data.len() as u64);
}

/// 断点续传：先写一半，再从断点续写，最终校验通过。
#[test]
fn resume_from_partial() {
    let c = temp_cache("resume", 1 << 30);
    let id = 42;
    let full = b"0123456789abcdef".to_vec();

    c.append(id, 0, &full[..8]).unwrap();
    assert_eq!(c.have_bytes(id, full.len() as u64), 8, "应从第 8 字节续传");

    c.append(id, 8, &full[8..]).unwrap();
    let expected = {
        let mut h = clipsync_core::hash::Hasher::new();
        h.update(&full);
        h.finish()
    };
    c.finalize(id, expected).unwrap();
    assert!(c.is_complete(id, full.len() as u64));
}

#[test]
fn out_of_order_chunk_is_rejected() {
    let c = temp_cache("ooo", 1 << 30);
    c.append(7, 0, b"abc").unwrap();
    // 偏移不连续应被拒绝，避免产生空洞导致内容损坏。
    assert!(c.append(7, 99, b"xyz").is_err());
}

#[test]
fn corrupted_content_fails_verification_and_is_discarded() {
    let c = temp_cache("corrupt", 1 << 30);
    let id = 9;
    c.append(id, 0, b"actual-content").unwrap();
    // 用错误的期望哈希模拟"源文件传输中被改动"。
    assert!(c.finalize(id, 0xDEADBEEF).is_err());
    // 损坏的分片必须被丢弃，不能留作缓存。
    assert_eq!(c.have_bytes(id, 14), 0);
}

#[test]
fn oversized_partial_is_discarded() {
    let c = temp_cache("oversize", 1 << 30);
    let id = 11;
    c.append(id, 0, b"way-too-much-data").unwrap();
    // 声明大小比已有内容小，说明是过期残留。
    assert_eq!(c.have_bytes(id, 4), 0);
}

#[test]
fn eviction_respects_limit_and_pins() {
    // 上限设为 10 字节，放入三个各 8 字节的 blob，必然要淘汰。
    let c = temp_cache("evict", 10);
    for id in [1u64, 2, 3] {
        c.append(id, 0, b"12345678").unwrap();
        let h = {
            let mut hh = clipsync_core::hash::Hasher::new();
            hh.update(b"12345678");
            hh.finish()
        };
        c.finalize(id, h).unwrap();
        // 让修改时间有先后差异，使 LRU 顺序确定。
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // 钉住 id=1（假设它正被剪贴板引用）。
    c.evict_to_limit(&[1]);

    assert!(c.is_complete(1, 8), "被钉住的内容不得淘汰");
    let remaining = [1u64, 2, 3].iter().filter(|id| c.is_complete(**id, 8)).count();
    assert!(remaining < 3, "超出上限后应有内容被淘汰");
}

#[test]
fn materialize_creates_named_files() {
    let c = temp_cache("materialize", 1 << 30);
    let id = 77;
    let data = b"file body";
    c.append(id, 0, data).unwrap();
    let h = {
        let mut hh = clipsync_core::hash::Hasher::new();
        hh.update(data);
        hh.finish()
    };
    c.finalize(id, h).unwrap();

    let paths = c.materialize(5, &[(id, "报告.pdf".to_string())]).unwrap();
    assert_eq!(paths.len(), 1);
    assert!(paths[0].exists());
    assert_eq!(std::fs::read(&paths[0]).unwrap(), data);
    assert!(paths[0].starts_with(c.received_dir()));
}

/// 对端提供的文件名不得逃逸落地目录（防目录穿越）。
#[test]
fn malicious_names_are_sanitized() {
    assert_eq!(sanitize_name("../../evil.exe"), "evil.exe");
    assert_eq!(sanitize_name(r"C:\Windows\system32\bad.dll"), "bad.dll");
    assert_eq!(sanitize_name("a<b>c.txt"), "a_b_c.txt");
    assert_eq!(sanitize_name("..."), "unnamed");
}

#[test]
fn cleanup_removes_old_generations_only() {
    let c = temp_cache("cleanup", 1 << 30);
    let id = 3;
    c.append(id, 0, b"x").unwrap();
    let h = {
        let mut hh = clipsync_core::hash::Hasher::new();
        hh.update(b"x");
        hh.finish()
    };
    c.finalize(id, h).unwrap();

    c.materialize(1, &[(id, "a.txt".into())]).unwrap();
    c.materialize(2, &[(id, "b.txt".into())]).unwrap();
    c.cleanup_materialized_except(Some(2));

    assert!(!c.received_dir().join(format!("{:016x}", 1)).exists());
    assert!(c.received_dir().join(format!("{:016x}", 2)).exists());
}
