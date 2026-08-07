//! 文件内容的发送侧：登记本机待发文件、流式分块发送、被新内容取代时立即中止。
//!
//! **为什么发送要"拉"而不是"推"**：若把 100MB 全部切块塞进发送通道，内存会被
//! 占满且无法中途取消。这里改为由连接的收发泵**按需拉取**——每轮循环读一块、
//! 发一块，天然受 TCP 背压约束，内存占用恒定为一个块的大小。
//!
//! **取代语义**：每轮发送前比对代际号。用户复制了新内容后代际号递增，正在进行
//! 的旧传输会立刻停止并通知对端——因为此刻剪贴板里已经不是那个文件了，继续
//! 传输只会让对端剪贴板与本机不一致。

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clipsync_core::SyncMessage;
use tracing::debug;

use crate::filecache::{hash_file, CHUNK_SIZE};

/// 本机可供对端索取的文件登记表：代际号 → [(文件标识, 本机路径)]。
///
/// 只保留最近若干代际——旧代际的内容已不在剪贴板里，对端不会再索取。
#[derive(Clone, Default)]
pub struct OutgoingFiles {
    inner: Arc<Mutex<HashMap<u64, Vec<(u64, PathBuf)>>>>,
    /// 当前代际号：由本地剪贴板变化递增，用于判断传输是否已被取代。
    current: Arc<AtomicU64>,
}

impl OutgoingFiles {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一次文件复制，并将其设为当前代际。
    pub fn register(&self, generation: u64, files: Vec<(u64, PathBuf)>) {
        let mut map = self.inner.lock().unwrap();
        map.insert(generation, files);
        // 只保留最近 4 代，防止长期运行后无限增长。
        if map.len() > 4 {
            let mut keys: Vec<u64> = map.keys().copied().collect();
            keys.sort_unstable();
            let drop_count = keys.len() - 4;
            for k in keys.into_iter().take(drop_count) {
                map.remove(&k);
            }
        }
        drop(map);
        self.current.store(generation, Ordering::SeqCst);
    }

    /// 剪贴板变成了非文件内容：推进代际号以中止正在进行的文件传输。
    pub fn advance(&self, generation: u64) {
        self.current.store(generation, Ordering::SeqCst);
    }

    /// 当前代际号。
    pub fn current(&self) -> u64 {
        self.current.load(Ordering::SeqCst)
    }

    /// 该代际是否仍是当前剪贴板内容。
    pub fn is_current(&self, generation: u64) -> bool {
        self.current() == generation
    }

    /// 查某代际下某文件的本机路径。
    pub fn path_for(&self, generation: u64, file_id: u64) -> Option<PathBuf> {
        self.inner
            .lock()
            .unwrap()
            .get(&generation)?
            .iter()
            .find(|(id, _)| *id == file_id)
            .map(|(_, p)| p.clone())
    }
}

/// 一个进行中的发送流：从某文件的指定偏移持续读出并发送。
pub struct OutgoingStream {
    generation: u64,
    file_id: u64,
    path: PathBuf,
    file: std::fs::File,
    offset: u64,
    buf: Vec<u8>,
    /// 本文件是否值得压缩（开流时采样判定一次，之后各块沿用）。
    compress: bool,
    /// 从头开始发送时，边读边算的内容哈希。
    ///
    /// `None` 表示这是一次**续传**（起始偏移不为 0）——前半段的字节我们根本
    /// 没读过，无从增量计算，只能在结尾重读整个文件。完整传输则不必：
    /// 反正每个字节都要过一遍手，顺手喂给哈希器就是了，省掉一次全量磁盘读。
    /// 90MB 的文件，这一次重读是实打实的开销。
    running_hash: Option<clipsync_core::hash::Hasher>,
}

impl OutgoingStream {
    /// 应对端请求开始发送某文件的 `offset` 之后的内容。
    ///
    /// `allow_compress` 为用户配置；实际是否压缩还要看内容采样结果——
    /// 已压缩的内容（jpg/mp4/zip）会自动跳过，不浪费 CPU。
    pub fn start(
        generation: u64,
        file_id: u64,
        path: PathBuf,
        offset: u64,
        allow_compress: bool,
    ) -> Result<Self> {
        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("打开待发送文件失败: {}", path.display()))?;
        file.seek(SeekFrom::Start(offset))
            .context("定位到续传偏移失败")?;

        let compress = allow_compress && crate::compress::is_worth_compressing(&path);
        if compress {
            debug!("文件 {} 判定为可压缩，将压缩后传输", path.display());
        }

        Ok(Self {
            generation,
            file_id,
            path,
            file,
            offset,
            buf: vec![0u8; CHUNK_SIZE],
            compress,
            // 只有从头发才能边读边算；续传缺了前半段，结尾仍需重读一遍。
            running_hash: (offset == 0).then(clipsync_core::hash::Hasher::new),
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 产出下一条待发消息。返回 `None` 表示本文件已发送完毕。
    ///
    /// `budget` 为本次允许读取的字节数上限（限速用）；传 `CHUNK_SIZE` 即不额外限制。
    /// 每次只读一个块，因此单次调用的内存与耗时都有界，便于在块之间响应
    /// 取代请求与其它剪贴板同步。
    pub fn next_message(&mut self, budget: usize) -> Result<Option<SyncMessage>> {
        let want = budget.min(self.buf.len()).max(1);
        let n = self
            .file
            .read(&mut self.buf[..want])
            .context("读取待发送文件失败")?;
        if n == 0 {
            // 读到结尾：给出整份内容的哈希供对端校验。
            //
            // 从头发的情况下，每个字节刚才都过了一遍手，增量算出来即可；
            // 续传则只读了后半段，无从增量，仍需重读整个文件。
            let content_hash = match self.running_hash.take() {
                Some(h) => h.finish(),
                None => hash_file(&self.path).context("计算发送文件哈希失败")?,
            };
            return Ok(Some(SyncMessage::FileDone {
                generation: self.generation,
                file_id: self.file_id,
                content_hash,
            }));
        }

        let plain = &self.buf[..n];
        if let Some(h) = self.running_hash.as_mut() {
            h.update(plain);
        }
        let (data, compressed) = if self.compress {
            match crate::compress::compress(plain) {
                // 压完反而更大就退回原始字节（极少见，但没必要白费带宽）。
                Ok(c) if c.len() < n => (c, true),
                _ => (plain.to_vec(), false),
            }
        } else {
            (plain.to_vec(), false)
        };

        let msg = SyncMessage::FileChunk {
            generation: self.generation,
            file_id: self.file_id,
            offset: self.offset,
            data,
            compressed,
            plain_len: n as u32,
        };
        self.offset += n as u64;
        Ok(Some(msg))
    }
}

/// 处理对端的文件索取请求，构造发送流。
///
/// 返回 `Ok(None)` 表示该请求已过期或文件不可用，调用方应回复相应消息。
pub fn begin_stream(
    outgoing: &OutgoingFiles,
    generation: u64,
    file_id: u64,
    offset: u64,
    allow_compress: bool,
) -> std::result::Result<OutgoingStream, SyncMessage> {
    // 请求的代际已被新剪贴板内容取代：告知对端放弃，不再浪费带宽。
    if !outgoing.is_current(generation) {
        debug!("忽略过期代际 {generation} 的文件请求（当前 {}）", outgoing.current());
        return Err(SyncMessage::FileAbort { generation });
    }
    let path = match outgoing.path_for(generation, file_id) {
        Some(p) => p,
        None => {
            return Err(SyncMessage::FileUnavailable {
                generation,
                file_id,
                reason: "该文件不在当前剪贴板内容中".into(),
            })
        }
    };
    OutgoingStream::start(generation, file_id, path.clone(), offset, allow_compress).map_err(|e| {
        SyncMessage::FileUnavailable {
            generation,
            file_id,
            reason: format!("无法读取文件 {}: {e}", path.display()),
        }
    })
}

#[cfg(test)]
mod tests {
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
        let mut s = OutgoingStream::start(1, 55, p, 0, false).unwrap();

        let m1 = s.next_message(CHUNK_SIZE).unwrap().unwrap();
        match m1 {
            SyncMessage::FileChunk { offset, ref data, .. } => {
                assert_eq!(offset, 0);
                assert_eq!(data.len(), CHUNK_SIZE);
            }
            _ => panic!("首条应为数据块"),
        }

        let m2 = s.next_message(CHUNK_SIZE).unwrap().unwrap();
        match m2 {
            SyncMessage::FileChunk { offset, ref data, .. } => {
                assert_eq!(offset, CHUNK_SIZE as u64);
                assert_eq!(data.len(), 100);
            }
            _ => panic!("次条应为剩余数据块"),
        }

        let m3 = s.next_message(CHUNK_SIZE).unwrap().unwrap();
        assert!(matches!(m3, SyncMessage::FileDone { .. }), "末条应为完成消息");
    }

    /// **关键正确性**：边读边算的哈希必须与重读整个文件算出的完全一致。
    ///
    /// 两者不一致的话，对端在 `finalize` 校验时会判定内容损坏并丢弃重传——
    /// 表现为文件永远同步不过去，而且日志里只说"校验失败"，根本想不到是
    /// 发送端算错了。用多块（跨越 CHUNK_SIZE 边界）来确保增量路径真的被走到。
    #[test]
    fn incremental_hash_matches_full_reread() {
        let data: Vec<u8> = (0..(CHUNK_SIZE * 2 + 1234)).map(|i| (i % 251) as u8).collect();
        let p = write_temp("hash_equiv.bin", &data);

        // 从头发送：走增量路径。
        let mut s = OutgoingStream::start(1, 1, p.clone(), 0, false).unwrap();
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

        let mut s = OutgoingStream::start(1, 1, p.clone(), 2000, false).unwrap();
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

        let mut s = OutgoingStream::start(1, 1, p.clone(), 0, true).unwrap();
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
            let mut s = OutgoingStream::start(1, 1, p.clone(), offset, false).unwrap();
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
        let mut s = OutgoingStream::start(1, 1, p, 6, false).unwrap();

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
        let mut s = OutgoingStream::start(1, 1, p, 0, false).unwrap();

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
        let mut s = OutgoingStream::start(1, 1, p, 0, true).unwrap();

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
        let mut s = OutgoingStream::start(1, 1, p, 0, false).unwrap();

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
        o.advance(2); // 剪贴板已更新

        match begin_stream(&o, 1, 1, 0, false) {
            Err(SyncMessage::FileAbort { generation }) => assert_eq!(generation, 1),
            _ => panic!("过期代际的请求应被中止"),
        }
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
}
