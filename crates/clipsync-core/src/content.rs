//! 剪贴板内容的平台无关表示。
//!
//! `clipsync-clip` 负责把各平台的原生剪贴板数据归一化成 `ClipContent`，
//! `clipsync-core::engine` 在此之上做过滤/去重/防回环，`clipsync-net`
//! 负责序列化传输。

use serde::{Deserialize, Serialize};

use crate::hash::Hasher;

/// 内容大类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentKind {
    Text,
    Image,
    Files,
}

/// 一张位图（统一为 RGBA8，行优先）。
///
/// 传输前可由 `clipsync-net` 压缩（如 PNG），此处保持解码后的裸格式
/// 以便平台层直接写入系统剪贴板。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageData {
    pub width: u32,
    pub height: u32,
    /// 长度应为 width * height * 4。
    pub rgba: Vec<u8>,
}

/// 剪贴板中的一个文件——**只含元数据，不含内容**。
///
/// 文件内容刻意不放进 `ClipContent`，原因有三：
///   1. **内存**：100MB 文件整个读进内存既浪费又无必要，内容应流式搬运。
///   2. **可取消**：内容若塞进一条大消息，就无法在传输中途被新剪贴板内容取代。
///   3. **可续传/去重**：内容按 `id` 分离存放，才能做缓存命中与断点续传。
///
/// 实际字节通过独立的分块传输流搬运（见 `SyncMessage::FileChunk`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    /// 文件名（不含目录，避免泄露源机目录结构）。
    pub name: String,
    /// 文件字节数。
    pub size: u64,
    /// 传输标识：由 (文件名, 大小, 修改时间) 派生，**无需读取文件内容**即可算出。
    ///
    /// 用途：缓存命中与断点续传的键。文件被修改后 (大小/修改时间变化) 标识自然
    /// 改变，旧缓存随之失效，因此不会用到脏数据。
    pub id: u64,
}

impl FileMeta {
    pub fn new(name: impl Into<String>, size: u64, id: u64) -> Self {
        Self {
            name: name.into(),
            size,
            id,
        }
    }
}

/// 归一化后的剪贴板内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipContent {
    Text(String),
    Image(ImageData),
    /// 文件列表（元数据）。内容另行分块传输。
    Files(Vec<FileMeta>),
}

impl ClipContent {
    pub fn kind(&self) -> ContentKind {
        match self {
            ClipContent::Text(_) => ContentKind::Text,
            ClipContent::Image(_) => ContentKind::Image,
            ClipContent::Files(_) => ContentKind::Files,
        }
    }

    /// 内容的字节大小，用于大小上限判定。
    ///
    /// 对文件而言是各文件大小之和——即将被搬运的实际数据量。
    pub fn byte_size(&self) -> usize {
        match self {
            ClipContent::Text(s) => s.len(),
            ClipContent::Image(img) => img.rgba.len(),
            ClipContent::Files(files) => files.iter().map(|f| f.size as usize).sum(),
        }
    }

    /// 确定性内容哈希：同一内容在任意设备上得到相同值。
    ///
    /// 用于防回环（写回系统剪贴板前登记哈希）与去重（跳过相同内容）。
    /// 哈希覆盖类型判别 + 全部字节，避免不同类型碰撞。
    /// 文件按元数据哈希（名称/大小/标识），无需读取内容。
    pub fn content_hash(&self) -> u64 {
        let mut h = Hasher::new();
        match self {
            ClipContent::Text(s) => {
                h.update(&[0]);
                h.update(s.as_bytes());
            }
            ClipContent::Image(img) => {
                h.update(&[1]);
                h.update(&img.width.to_le_bytes());
                h.update(&img.height.to_le_bytes());
                h.update(&img.rgba);
            }
            ClipContent::Files(files) => {
                h.update(&[2]);
                for f in files {
                    h.update(&(f.name.len() as u64).to_le_bytes());
                    h.update(f.name.as_bytes());
                    h.update(&f.size.to_le_bytes());
                    h.update(&f.id.to_le_bytes());
                }
            }
        }
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img() -> ImageData {
        ImageData {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 4],
        }
    }

    #[test]
    fn kind_matches_variant() {
        assert_eq!(ClipContent::Text("x".into()).kind(), ContentKind::Text);
        assert_eq!(ClipContent::Image(img()).kind(), ContentKind::Image);
        assert_eq!(ClipContent::Files(vec![]).kind(), ContentKind::Files);
    }

    #[test]
    fn hash_is_deterministic() {
        let a = ClipContent::Text("hello".into());
        let b = ClipContent::Text("hello".into());
        assert_eq!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn hash_distinguishes_content() {
        assert_ne!(
            ClipContent::Text("hello".into()).content_hash(),
            ClipContent::Text("world".into()).content_hash()
        );
    }

    #[test]
    fn hash_distinguishes_kind_for_same_bytes() {
        // 文本 "\x01" 与某图片不应仅因字节相近而碰撞：类型前缀保证区分。
        let t = ClipContent::Text(String::from("A"));
        let i = ClipContent::Image(img());
        assert_ne!(t.content_hash(), i.content_hash());
    }

    #[test]
    fn byte_size_sums_files() {
        let c = ClipContent::Files(vec![FileMeta::new("a", 10, 1), FileMeta::new("b", 5, 2)]);
        assert_eq!(c.byte_size(), 15);
    }

    /// 文件哈希只依据元数据，不需读取内容——保证复制大文件时不产生磁盘 IO。
    #[test]
    fn file_hash_uses_metadata_only() {
        let a = ClipContent::Files(vec![FileMeta::new("doc.pdf", 1024, 99)]);
        let b = ClipContent::Files(vec![FileMeta::new("doc.pdf", 1024, 99)]);
        assert_eq!(a.content_hash(), b.content_hash());

        // 标识变化（文件被修改）应产生不同哈希，使旧内容不被误判为重复。
        let c = ClipContent::Files(vec![FileMeta::new("doc.pdf", 1024, 100)]);
        assert_ne!(a.content_hash(), c.content_hash());

        // 大小变化同样应改变哈希。
        let d = ClipContent::Files(vec![FileMeta::new("doc.pdf", 2048, 99)]);
        assert_ne!(a.content_hash(), d.content_hash());
    }

    #[test]
    fn file_order_affects_hash() {
        let a = ClipContent::Files(vec![FileMeta::new("a", 1, 1), FileMeta::new("b", 2, 2)]);
        let b = ClipContent::Files(vec![FileMeta::new("b", 2, 2), FileMeta::new("a", 1, 1)]);
        assert_ne!(a.content_hash(), b.content_hash());
    }
}
