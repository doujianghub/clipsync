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
    /// 文件总字节数（开流时定格），用于报进度。
    size: u64,
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

        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
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
            size,
            buf: vec![0u8; CHUNK_SIZE],
            compress,
            // 只有从头发才能边读边算；续传缺了前半段，结尾仍需重读一遍。
            running_hash: (offset == 0).then(clipsync_core::hash::Hasher::new),
        })
    }

    /// 当前进度：(已发字节, 总字节) 与文件名，供托盘显示。
    pub fn progress(&self) -> (u64, u64, String) {
        let name = self
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "文件".into());
        (self.offset, self.size, name)
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
#[path = "filetransfer_tests.rs"]
mod tests;
