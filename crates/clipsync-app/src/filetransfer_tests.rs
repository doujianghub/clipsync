//! `filetransfer` 的单元测试。
//!
//! 单独成文件只为让 `filetransfer.rs` 保持在项目约定的行数以内；
//! 它仍是 `filetransfer` 的子模块（由 `#[path]` 引入），`use super::*`
//! 照常可用，与写在原文件里没有区别。

use super::*;

fn write_temp(tag: &str, data: &[u8]) -> PathBuf {
    let dir = std::env::temp_dir().join("ClipSyncOutTest");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(tag);
    std::fs::write(&p, data).unwrap();
    p
}

#[test]
fn registers_and_finds_paths() {
    let o = OutgoingFiles::new();
    let p = write_temp("reg.bin", b"x");
    o.register(7, vec![(123, p.clone())]);

    assert_eq!(o.current(), 7);
    assert!(o.is_current(7));
    assert_eq!(o.path_for(7, 123), Some(p));
    assert_eq!(o.path_for(7, 999), None);
}

#[test]
fn advancing_generation_invalidates_old() {
    let o = OutgoingFiles::new();
    o.register(1, vec![]);
    assert!(o.is_current(1));
    // 用户复制了别的内容。
    o.advance(2);
    assert!(!o.is_current(1), "旧代际应立即失效，使进行中的传输中止");
}

#[test]
fn only_recent_generations_are_kept() {
    let o = OutgoingFiles::new();
    let p = write_temp("keep.bin", b"y");
    for gen in 1..=6u64 {
        o.register(gen, vec![(gen, p.clone())]);
    }
    // 最早的代际应已被清理，避免无限增长。
    assert!(o.path_for(1, 1).is_none());
    assert!(o.path_for(6, 6).is_some());
}

#[test]
fn stream_emits_chunks_then_done() {
    // 构造略大于一个块的数据，确保产生多块。
    let data = vec![9u8; CHUNK_SIZE + 100];
    let p = write_temp("stream.bin", &data);
    // 关闭压缩，便于直接断言原始长度。
    let mut s = OutgoingStream::start(1, 55, p, 0, false, true).unwrap();

    let m1 = s.next_message(CHUNK_SIZE).unwrap().unwrap();
    match m1 {
        SyncMessage::FileChunk {
            offset, ref data, ..
        } => {
            assert_eq!(offset, 0);
            assert_eq!(data.len(), CHUNK_SIZE);
        }
        _ => panic!("首条应为数据块"),
    }

    let m2 = s.next_message(CHUNK_SIZE).unwrap().unwrap();
    match m2 {
        SyncMessage::FileChunk {
            offset, ref data, ..
        } => {
            assert_eq!(offset, CHUNK_SIZE as u64);
            assert_eq!(data.len(), 100);
        }
        _ => panic!("次条应为剩余数据块"),
    }

    let m3 = s.next_message(CHUNK_SIZE).unwrap().unwrap();
    assert!(
        matches!(m3, SyncMessage::FileDone { .. }),
        "末条应为完成消息"
    );
}

/// **关键正确性**：边读边算的哈希必须与重读整个文件算出的完全一致。
///
/// 两者不一致的话，对端在 `finalize` 校验时会判定内容损坏并丢弃重传——
/// 表现为文件永远同步不过去，而且日志里只说"校验失败"，根本想不到是
/// 发送端算错了。用多块（跨越 CHUNK_SIZE 边界）来确保增量路径真的被走到。
#[test]
fn incremental_hash_matches_full_reread() {
    let data: Vec<u8> = (0..(CHUNK_SIZE * 2 + 1234))
        .map(|i| (i % 251) as u8)
        .collect();
    let p = write_temp("hash_equiv.bin", &data);

    // 从头发送：走增量路径。
    let mut s = OutgoingStream::start(1, 1, p.clone(), 0, false, true).unwrap();
    let mut incremental = None;
    while let Some(msg) = s.next_message(CHUNK_SIZE).unwrap() {
        if let SyncMessage::FileDone { content_hash, .. } = msg {
            incremental = Some(content_hash);
            break;
        }
    }

    let full = hash_file(&p).unwrap();
    assert_eq!(
        incremental.expect("应产出 FileDone"),
        full,
        "增量哈希与重读结果不一致——对端会判定内容损坏并永远重传"
    );
}

/// 续传路径没有前半段字节，必须退回重读整个文件，且结果同样正确。
#[test]
fn resumed_transfer_still_hashes_whole_file() {
    let data: Vec<u8> = (0..5000).map(|i| (i % 97) as u8).collect();
    let p = write_temp("hash_resume.bin", &data);

    let mut s = OutgoingStream::start(1, 1, p.clone(), 2000, false, true).unwrap();
    assert!(
        s.running_hash.is_none(),
        "续传不该启用增量哈希——前 2000 字节根本没读过"
    );

    let mut got = None;
    while let Some(msg) = s.next_message(CHUNK_SIZE).unwrap() {
        if let SyncMessage::FileDone { content_hash, .. } = msg {
            got = Some(content_hash);
            break;
        }
    }
    assert_eq!(
        got.unwrap(),
        hash_file(&p).unwrap(),
        "续传给出的必须是**整个文件**的哈希，不是后半段的"
    );
}

/// 压缩开启时哈希仍应基于**原始**字节，而不是压缩后的。
#[test]
fn hash_covers_plaintext_not_compressed_bytes() {
    let data = vec![b'Z'; 200_000]; // 高度可压
    let p = write_temp("hash_compressed.bin", &data);

    let mut s = OutgoingStream::start(1, 1, p.clone(), 0, true, true).unwrap();
    let mut got = None;
    while let Some(msg) = s.next_message(CHUNK_SIZE).unwrap() {
        if let SyncMessage::FileDone { content_hash, .. } = msg {
            got = Some(content_hash);
            break;
        }
    }
    assert_eq!(
        got.unwrap(),
        hash_file(&p).unwrap(),
        "压缩不该影响内容哈希——对端解压后校验的是原始内容"
    );
}

/// 手动基准：量化"省掉一次全量重读"到底值多少。
///
/// 默认 `#[ignore]`——它要写一个 90MB 的临时文件，不适合每次 `cargo test`
/// 都跑。**必须用优化构建**，debug 下哈希慢一个数量级会把差异淹掉：
///
/// ```text
/// cargo test --release -p clipsync-app --bin clipsync -- --ignored hash_benchmark --nocapture
/// ```
#[test]
#[ignore = "会写 90MB 临时文件，且需 --release 才有意义"]
fn hash_benchmark() {
    let size = 90 * 1024 * 1024;
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let p = write_temp("hash_bench.bin", &data);

    let run = |offset: u64| {
        let t = std::time::Instant::now();
        let mut s = OutgoingStream::start(1, 1, p.clone(), offset, false, true).unwrap();
        while let Some(m) = s.next_message(CHUNK_SIZE).unwrap() {
            if matches!(m, SyncMessage::FileDone { .. }) {
                break;
            }
        }
        t.elapsed()
    };

    let incremental = run(0); // 边读边算
    let reread = run(1); // 续传路径：结尾重读整个文件

    println!("90MB 文件：");
    println!("  边读边算（完整传输）: {incremental:?}");
    println!("  重读一遍（续传路径）: {reread:?}");
    println!(
        "  省下: {:?}（{:.0}%）",
        reread.saturating_sub(incremental),
        (reread.as_secs_f64() - incremental.as_secs_f64()) / reread.as_secs_f64() * 100.0
    );

    let _ = std::fs::remove_file(&p);
}

/// 续传：从指定偏移开始只发送剩余部分。
#[test]
fn stream_resumes_from_offset() {
    let data = b"0123456789".to_vec();
    let p = write_temp("resume.bin", &data);
    let mut s = OutgoingStream::start(1, 1, p, 6, false, true).unwrap();

    match s.next_message(CHUNK_SIZE).unwrap().unwrap() {
        SyncMessage::FileChunk { offset, data, .. } => {
            assert_eq!(offset, 6);
            assert_eq!(data, b"6789".to_vec(), "只应发送断点之后的内容");
        }
        _ => panic!("应为数据块"),
    }
}

/// 限速：`budget` 限制单次读取量，偏移随实际发送量推进。
#[test]
fn budget_limits_chunk_size() {
    let data = vec![7u8; 10_000];
    let p = write_temp("budget.bin", &data);
    let mut s = OutgoingStream::start(1, 1, p, 0, false, true).unwrap();

    match s.next_message(1000).unwrap().unwrap() {
        SyncMessage::FileChunk { offset, data, .. } => {
            assert_eq!(offset, 0);
            assert_eq!(data.len(), 1000, "单次发送量应受 budget 限制");
        }
        _ => panic!("应为数据块"),
    }
    // 下一块应接在前一块之后。
    match s.next_message(1000).unwrap().unwrap() {
        SyncMessage::FileChunk { offset, .. } => assert_eq!(offset, 1000),
        _ => panic!("应为数据块"),
    }
}

/// 可压缩内容应被压缩发送，且标记正确、原始长度如实上报。
#[test]
fn compressible_content_is_compressed() {
    let data = vec![b'A'; 100_000]; // 高度重复，必然可压
    let p = write_temp("compressible.bin", &data);
    let mut s = OutgoingStream::start(1, 1, p, 0, true, true).unwrap();

    match s.next_message(CHUNK_SIZE).unwrap().unwrap() {
        SyncMessage::FileChunk {
            data: sent,
            compressed,
            plain_len,
            ..
        } => {
            assert!(compressed, "重复内容应被压缩");
            assert_eq!(plain_len, 100_000, "应如实上报解压后长度");
            assert!(sent.len() < 100_000 / 10, "压缩后应显著变小");
            // 解压应还原原始内容。
            let back = crate::compress::decompress(&sent, plain_len as usize).unwrap();
            assert_eq!(back, data);
        }
        _ => panic!("应为数据块"),
    }
}

/// 关闭压缩配置时，即使内容可压也按原样发送。
#[test]
fn compression_can_be_disabled() {
    let data = vec![b'B'; 50_000];
    let p = write_temp("nocompress.bin", &data);
    let mut s = OutgoingStream::start(1, 1, p, 0, false, true).unwrap();

    match s.next_message(CHUNK_SIZE).unwrap().unwrap() {
        SyncMessage::FileChunk {
            data: sent,
            compressed,
            ..
        } => {
            assert!(!compressed);
            assert_eq!(sent.len(), 50_000, "关闭压缩时应原样发送");
        }
        _ => panic!("应为数据块"),
    }
}

#[test]
fn stale_generation_request_is_aborted() {
    let o = OutgoingFiles::new();
    o.register(1, vec![(1, write_temp("stale.bin", b"z"))]);
    // 又复制了一个**文件**：上一份才真的不再提供。
    o.register(2, vec![(2, write_temp("newer.bin", b"z"))]);

    match begin_stream(&o, 1, 1, 0, false) {
        Err(SyncMessage::FileAbort { generation }) => assert_eq!(generation, 1),
        _ => panic!("被新的文件复制取代之后，旧代际的请求应被中止"),
    }
}

/// 复制了文字之后，那份大文件**仍然拿得到**——这是"延后取回"的前提。
///
/// 超过对方自动取回上限的文件会挂在它那儿等人点，而这期间本机很可能已经
/// 复制过文字、截过图。若照 `is_current` 判定，对方那个「取回」按钮基本上
/// 一按一个空。
#[test]
fn a_file_stays_servable_after_copying_text() {
    let o = OutgoingFiles::new();
    o.register(1, vec![(1, write_temp("big.bin", b"payload"))]);
    o.advance(2); // 复制了一段文字
    o.advance(3); // 又复制了一段

    assert!(!o.is_current(1), "它确实已不是当前剪贴板内容");
    assert!(o.is_servable(1), "但仍该拿得到——对方可能正等着手动取回");
    assert!(
        begin_stream(&o, 1, 1, 0, false).is_ok(),
        "手动取回必须能开流"
    );
}

/// 开流时若请求的已不是当前内容，那只能是手动取回——它不受"剪贴板变了就
/// 中止"的约束。
///
/// 自动那条路在收到 `Clip` 的当场就发 `FileNeed`，那时它必然还是当前内容；
/// 所以开流那一刻的 `is_current` 恰好能把两者分开，无需改协议加字段。
#[test]
fn manual_fetch_streams_do_not_abort_on_supersede() {
    let o = OutgoingFiles::new();
    o.register(1, vec![(1, write_temp("manual.bin", b"payload"))]);

    let auto = begin_stream(&o, 1, 1, 0, false).expect("当前内容应能开流");
    assert!(auto.abort_on_supersede(), "自动传输：剪贴板一变就该收手");

    o.advance(2); // 本机复制了别的东西
    let manual = begin_stream(&o, 1, 1, 0, false).expect("延后取回仍应能开流");
    assert!(
        !manual.abort_on_supersede(),
        "人明确点了取回，要的就是这一份，本机剪贴板换成什么与他无关"
    );
}

#[test]
fn missing_file_reports_unavailable() {
    let o = OutgoingFiles::new();
    o.register(3, vec![(1, PathBuf::from("/definitely/missing/file.bin"))]);

    match begin_stream(&o, 3, 1, 0, false) {
        Err(SyncMessage::FileUnavailable { file_id, .. }) => assert_eq!(file_id, 1),
        _ => panic!("不可读的文件应报告 FileUnavailable"),
    }
}

#[test]
fn unknown_file_id_reports_unavailable() {
    let o = OutgoingFiles::new();
    o.register(4, vec![]);
    assert!(matches!(
        begin_stream(&o, 4, 12345, 0, false),
        Err(SyncMessage::FileUnavailable { .. })
    ));
}
