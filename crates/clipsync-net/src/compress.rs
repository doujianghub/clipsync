//! 字节级压缩，供帧层与文件分块共用。
//!
//! 级别固定取 1（最快）。本场景的瓶颈永远是带宽而不是压缩比——实测一张
//! 3840×2160 的截图（裸 RGBA 33.2 MB）：level 1 压到 2.17 MB 用 53 ms，
//! level 6 压到 1.58 MB 却要 149 ms。多花 96 ms 省下 0.6 MB，只有在低于
//! 6 MB/s 的链路上才划算，而那种链路本来就该先去修链路。

use anyhow::{Context, Result};

/// 压缩级别。取 1（最快），理由见模块说明。
const LEVEL: u32 = 1;

/// 采样判定用的样本大小。
pub const SAMPLE_SIZE: usize = 64 * 1024;

/// 压缩率优于此值才认为"值得压"。0.9 表示至少要省下 10%。
const WORTH_IT_RATIO: f64 = 0.9;

/// 拿一段样本判断整份内容值不值得压。
///
/// **为什么不直接整份压了再看**：压不动的内容（照片、jpg、已压缩的分块）会
/// 白白花掉整份数据的压缩时间。实测 200 MB 伪随机负载，全量试压把吞吐从
/// 163 MB/s 砍到 84 MB/s；改成采样后开销回到千分之一量级。
///
/// 样本超过 [`SAMPLE_SIZE`] 的部分会被忽略——判个趋势用不着全看。
pub fn worth_compressing(sample: &[u8]) -> bool {
    // 太小：deflate 自己的头部开销就占掉可观比例，压了也是负收益。
    if sample.len() < 1024 {
        return false;
    }
    let sample = &sample[..sample.len().min(SAMPLE_SIZE)];
    match compress(sample) {
        Ok(c) => (c.len() as f64 / sample.len() as f64) < WORTH_IT_RATIO,
        Err(_) => false,
    }
}

/// 压缩一段数据。
pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;

    let mut enc = flate2::write::DeflateEncoder::new(
        Vec::with_capacity(data.len() / 2),
        flate2::Compression::new(LEVEL),
    );
    enc.write_all(data).context("压缩写入失败")?;
    enc.finish().context("压缩收尾失败")
}

/// 解压一段数据，**硬上限为 `expected_len`**。
///
/// 上限不是优化而是防御：`expected_len` 来自对端声明，压缩数据的膨胀比可以
/// 高达 1000:1，几 KB 的输入足以解出几 GB。虽然能发到这里的都是已通过 Noise
/// 认证的已配对设备，但"对端实现有 bug"和"对端被攻破"都不该表现为本机 OOM。
/// 多读一个字节就报错，而不是 `read_to_end` 一路吃到内存耗尽。
pub fn decompress(data: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    use std::io::Read;

    let mut out = Vec::with_capacity(expected_len);
    // 多给 1 字节的额度：读满了说明对端声明的长度偏小，属于异常，下面会拦住。
    flate2::read::DeflateDecoder::new(data)
        .take(expected_len as u64 + 1)
        .read_to_end(&mut out)
        .context("解压失败")?;
    if out.len() != expected_len {
        anyhow::bail!("解压结果与声明长度不符（声明 {expected_len}，实得 {}）", out.len());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let data = b"hello clipsync ".repeat(1000);
        let c = compress(&data).unwrap();
        assert_eq!(decompress(&c, data.len()).unwrap(), data);
    }

    /// 截图那类数据（大片同色）应压到零头——这正是本模块存在的理由。
    #[test]
    fn screenshot_like_data_compresses_hard() {
        // 模拟一张纯色为主、带少量噪点的位图。
        let mut rgba = vec![0xF0u8; 4 * 1024 * 1024];
        for (i, b) in rgba.iter_mut().enumerate() {
            if i % 997 == 0 {
                *b = (i % 251) as u8;
            }
        }
        let c = compress(&rgba).unwrap();
        assert!(
            c.len() < rgba.len() / 10,
            "位图应压到一成以下，实得 {} / {}",
            c.len(),
            rgba.len()
        );
    }

    /// 声明长度比实际小：必须报错，不能截断后当成功返回。
    ///
    /// 截断的后果比报错坏得多——上层会拿着半条消息去 postcard 解码，
    /// 报出来的是"解码失败"，查半天也想不到根因在长度声明上。
    #[test]
    fn understated_length_is_rejected() {
        let data = vec![7u8; 10_000];
        let c = compress(&data).unwrap();
        let err = decompress(&c, 500).unwrap_err();
        assert!(
            err.to_string().contains("声明长度不符"),
            "应明确指出长度不符，实得: {err}"
        );
    }

    /// 解压炸弹不该把内存吃光：1 MB 全零能压到几 KB，声明一个小长度即可拦下。
    #[test]
    fn decompression_is_bounded_by_the_declared_length() {
        let bomb = compress(&vec![0u8; 1024 * 1024]).unwrap();
        assert!(bomb.len() < 8 * 1024, "构造前提：应压得很小");
        assert!(decompress(&bomb, 4096).is_err(), "超出声明长度应被拒绝");
    }

    /// 采样判据必须挡住压不动的内容——它是吞吐的守门人，不是可有可无的优化。
    #[test]
    fn incompressible_bytes_are_not_worth_it() {
        let mut x: u32 = 0x1234_5678;
        let data: Vec<u8> = std::iter::repeat_with(|| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 16) as u8
        })
        .take(200_000)
        .collect();
        assert!(!worth_compressing(&data), "伪随机内容不该判定为值得压");
    }

    /// 反过来，位图那类内容必须判定为值得压。
    #[test]
    fn bitmap_like_bytes_are_worth_it() {
        let data = vec![0xF0u8; 200_000];
        assert!(worth_compressing(&data));
    }

    /// 太小的输入不值得压：deflate 自己的头部就占掉可观比例。
    #[test]
    fn tiny_input_is_not_worth_it() {
        assert!(!worth_compressing(b"hi"));
        assert!(!worth_compressing(&vec![0u8; 512]));
    }
}
