//! 文件传输前的**自适应**压缩判定。
//!
//! **为什么值得做**：文本、代码、日志、CSV 等常压到原体积的 20-35%，等于把
//! 有效吞吐翻数倍；更重要的是它**减少**带宽占用，在慢链路上收益最大。
//!
//! **为什么要"自适应"**：JPEG/MP4/ZIP 等已压缩内容再压几乎无收益，白费 CPU
//! 还可能变大。因此先用文件开头的一小段试压，压不动就整份走原始字节。
//!
//! **逐块独立压缩**：每个分块单独压缩、单独标记。这样断点续传与分块并行传输
//! 都不受影响——`offset` 始终指**原始文件**中的位置，与是否压缩无关。
//!
//! 压缩本身在 [`clipsync_net::compress`]，与帧层共用一份实现；本模块只负责
//! "这个文件值不值得压"这个判断——它要读盘采样，属于 app 层的事。

pub use clipsync_net::compress::{compress, decompress};

/// 判断某文件是否值得在传输时压缩。
///
/// 读开头一段试压，判据与帧层完全一致（同一个
/// [`worth_compressing`](clipsync_net::compress::worth_compressing)）——两处
/// 用不同的阈值只会让"为什么这个文件压了那个没压"变成一个谜。
///
/// 文件读取失败时保守返回 `false`：压缩是优化，读不到就按不压处理，
/// 真正的读取失败会在实际传输时报出来，不该由这里代为报错。
pub fn is_worth_compressing(path: &std::path::Path) -> bool {
    use std::io::Read;

    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut sample = vec![0u8; clipsync_net::compress::SAMPLE_SIZE];
    let n = match f.read(&mut sample) {
        Ok(n) => n,
        Err(_) => return false,
    };
    sample.truncate(n);
    clipsync_net::compress::worth_compressing(&sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worth_compressing_detects_text_file() {
        let dir = std::env::temp_dir().join("ClipSyncCompressTest");
        std::fs::create_dir_all(&dir).unwrap();

        let text = dir.join("text.txt");
        std::fs::write(&text, "the quick brown fox ".repeat(5000)).unwrap();
        assert!(is_worth_compressing(&text), "文本文件应判定为值得压缩");
    }

    #[test]
    fn worth_compressing_rejects_incompressible_file() {
        let dir = std::env::temp_dir().join("ClipSyncCompressTest");
        std::fs::create_dir_all(&dir).unwrap();

        // 伪随机内容模拟 jpg/mp4/zip。
        let mut data = Vec::with_capacity(200_000);
        let mut x: u32 = 0xDEADBEEF;
        for _ in 0..200_000 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            data.push((x >> 16) as u8);
        }
        let bin = dir.join("random.bin");
        std::fs::write(&bin, &data).unwrap();
        assert!(
            !is_worth_compressing(&bin),
            "已压缩类内容不应判定为值得压缩，否则白费 CPU"
        );
    }

    #[test]
    fn tiny_file_is_not_worth_compressing() {
        let dir = std::env::temp_dir().join("ClipSyncCompressTest");
        std::fs::create_dir_all(&dir).unwrap();
        let tiny = dir.join("tiny.txt");
        std::fs::write(&tiny, "hi").unwrap();
        assert!(!is_worth_compressing(&tiny));
    }

    #[test]
    fn missing_file_is_not_worth_compressing() {
        assert!(!is_worth_compressing(std::path::Path::new(
            "/definitely/missing/xyz.bin"
        )));
    }
}
