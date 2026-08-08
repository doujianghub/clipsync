//! 文件内容缓存：断点续传、重复内容秒同步、落地为可粘贴的真实文件。
//!
//! **目录结构**（均在系统临时目录下）：
//! ```text
//! <temp>/ClipSync/
//!   parts/<id>.part   接收中的部分内容（可续传）
//!   blobs/<id>.bin    已完整接收并校验通过的内容
//!   recv/<gen>/<名字> 落地给用户粘贴的文件（尽量用硬链接指向 blob，不占额外空间）
//! ```
//!
//! **为什么分 part / blob 两级**：只有校验通过的内容才会成为 blob，因此绝不会
//! 把损坏或过期的数据当作有效缓存交给用户。
//!
//! **空间控制**：blobs 与 parts 总量超过上限时按最久未使用淘汰；当前剪贴板
//! 正在引用的内容会被"钉住"不淘汰（否则用户一粘贴就发现文件没了）。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{debug, warn};

/// 分块大小。256KB 在"减少消息开销"与"能及时响应取代/插入其它同步"之间平衡：
/// 千兆局域网上一块约 2ms，取消一次传输的最坏延迟即为此量级。
pub const CHUNK_SIZE: usize = 256 * 1024;

/// 文件内容缓存。
pub struct FileCache {
    root: PathBuf,
    /// 缓存总量上限（字节）。
    limit: u64,
}

impl FileCache {
    /// 在系统临时目录下建立缓存。
    pub fn new(limit: u64) -> Result<Self> {
        let root = std::env::temp_dir().join("ClipSync");
        for sub in ["parts", "blobs", "recv"] {
            std::fs::create_dir_all(root.join(sub))
                .with_context(|| format!("创建缓存目录失败: {}", root.join(sub).display()))?;
        }
        Ok(Self { root, limit })
    }

    /// 落地文件所在的根目录。
    ///
    /// 上层用它识别"这是本程序自己落地的接收文件"，从而不会把收到的文件又
    /// 广播回去形成回环。
    pub fn received_dir(&self) -> PathBuf {
        self.root.join("recv")
    }

    fn part_path(&self, id: u64) -> PathBuf {
        self.root.join("parts").join(format!("{id:016x}.part"))
    }

    fn blob_path(&self, id: u64) -> PathBuf {
        self.root.join("blobs").join(format!("{id:016x}.bin"))
    }

    /// 该内容是否已完整缓存（可秒同步，无需任何传输）。
    pub fn is_complete(&self, id: u64, expected_size: u64) -> bool {
        std::fs::metadata(self.blob_path(id))
            .map(|m| m.len() == expected_size)
            .unwrap_or(false)
    }

    /// 已持有的字节数——即断点续传的起点。
    ///
    /// 完整命中返回文件大小；有部分内容返回已有长度；没有则返回 0。
    pub fn have_bytes(&self, id: u64, expected_size: u64) -> u64 {
        if self.is_complete(id, expected_size) {
            return expected_size;
        }
        let have = std::fs::metadata(self.part_path(id))
            .map(|m| m.len())
            .unwrap_or(0);
        // 部分内容比声明的还大，说明是过期残留，丢弃重来。
        if have > expected_size {
            let _ = std::fs::remove_file(self.part_path(id));
            return 0;
        }
        have
    }

    /// 追加一段内容到部分文件。`offset` 必须等于当前已有长度，否则视为乱序丢弃。
    pub fn append(&self, id: u64, offset: u64, data: &[u8]) -> Result<u64> {
        use std::io::Write;

        let path = self.part_path(id);
        let current = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if offset != current {
            anyhow::bail!("分块偏移不连续（期望 {current}，收到 {offset}），丢弃");
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("打开缓存分片失败: {}", path.display()))?;
        f.write_all(data).context("写入缓存分片失败")?;
        Ok(current + data.len() as u64)
    }

    /// 校验部分文件的内容哈希并转正为 blob。
    ///
    /// 校验失败（文件在传输途中被改动等）会丢弃分片并报错，绝不产生损坏文件。
    pub fn finalize(&self, id: u64, expected_hash: u64) -> Result<()> {
        let part = self.part_path(id);
        let actual = hash_file(&part).context("计算缓存内容哈希失败")?;
        if actual != expected_hash {
            let _ = std::fs::remove_file(&part);
            anyhow::bail!("内容校验失败（源文件可能在传输中被修改），已丢弃并将重传");
        }
        let blob = self.blob_path(id);
        std::fs::rename(&part, &blob)
            .with_context(|| format!("转存缓存失败: {}", blob.display()))?;
        Ok(())
    }

    /// 把若干已缓存的内容落地为带真实文件名、可被粘贴的文件。
    ///
    /// 优先用硬链接指向 blob——同卷情况下不占额外空间也不需要复制；失败
    /// （跨卷/文件系统不支持）再退回复制。
    pub fn materialize(&self, generation: u64, files: &[(u64, String)]) -> Result<Vec<PathBuf>> {
        let dir = self.received_dir().join(format!("{generation:016x}"));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("创建落地目录失败: {}", dir.display()))?;

        let mut out = Vec::with_capacity(files.len());
        for (id, name) in files {
            let blob = self.blob_path(*id);
            let dest = dir.join(sanitize_name(name));
            // 已存在则先移除，避免硬链接失败。
            let _ = std::fs::remove_file(&dest);
            if std::fs::hard_link(&blob, &dest).is_err() {
                std::fs::copy(&blob, &dest).with_context(|| {
                    format!("落地文件失败: {} → {}", blob.display(), dest.display())
                })?;
            }
            out.push(dest);
        }
        Ok(out)
    }

    /// 清理上一批落地文件（新内容取代旧内容后，旧的不再需要）。
    ///
    /// 只删除本程序自己创建的落地目录，不触碰任何用户文件。
    pub fn cleanup_materialized_except(&self, keep_generation: Option<u64>) {
        let recv = self.received_dir();
        let keep = keep_generation.map(|g| format!("{g:016x}"));
        let entries = match std::fs::read_dir(&recv) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if Some(name.as_ref()) == keep.as_deref() {
                continue;
            }
            if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                debug!("清理旧落地目录失败 {}: {e}", entry.path().display());
            }
        }
    }

    /// 按最久未使用淘汰缓存，直到总量回到上限内。
    ///
    /// `pinned` 中的内容不会被淘汰——它们正被当前剪贴板引用，删了会导致粘贴失败。
    pub fn evict_to_limit(&self, pinned: &[u64]) {
        let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
        let mut total: u64 = 0;

        for sub in ["blobs", "parts"] {
            let dir = self.root.join(sub);
            let rd = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for e in rd.flatten() {
                let md = match e.metadata() {
                    Ok(m) if m.is_file() => m,
                    _ => continue,
                };
                total += md.len();
                let atime = md
                    .accessed()
                    .or_else(|_| md.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                entries.push((e.path(), md.len(), atime));
            }
        }

        if total <= self.limit {
            return;
        }

        // 最久未使用者排前面。
        entries.sort_by_key(|(_, _, t)| *t);
        let pinned_names: Vec<String> = pinned.iter().map(|id| format!("{id:016x}")).collect();

        for (path, size, _) in entries {
            if total <= self.limit {
                break;
            }
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if pinned_names.contains(&stem) {
                continue; // 正在使用，不能删
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    total = total.saturating_sub(size);
                    debug!("淘汰缓存 {}（{} 字节）", path.display(), size);
                }
                Err(e) => warn!("淘汰缓存失败 {}: {e}", path.display()),
            }
        }
    }
}

/// 流式计算文件内容哈希，恒定内存占用。
pub fn hash_file(path: &Path) -> Result<u64> {
    use std::io::Read;

    let mut f =
        std::fs::File::open(path).with_context(|| format!("打开文件失败: {}", path.display()))?;
    let mut hasher = clipsync_core::hash::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).context("读取文件失败")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finish())
}

/// 去掉文件名中的路径成分与非法字符，防止对端提供的名字逃逸出落地目录。
fn sanitize_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim_matches('.');
    let cleaned: String = base
        .chars()
        .map(|c| if r#"<>:"|?*"#.contains(c) { '_' } else { c })
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
#[path = "filecache_tests.rs"]
mod tests;
