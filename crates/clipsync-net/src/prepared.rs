//! 消息的**线上表示**：序列化并按需压缩后的字节，可跨连接共享。
//!
//! **为什么要单独有这么个类型**：广播是一份内容发给 N 台设备。压缩结果只跟
//! 内容有关、跟发给谁无关，可每条连接各自 `send` 就会各压一遍——两台设备时
//! 日志里能看到两行一模一样的「帧层压缩 33129653 → 3174747」。一张 4K 截图
//! 每份要 15 ms 序列化加 40 ms deflate 再加一份 33 MB 的内存副本，而且是
//! O(N)：设备越多，最后一台等得越久。
//!
//! 加密则**不能**这样共享：每条 Noise 连接有独立的会话密钥与 nonce 序列，
//! 同一份密文发给两台设备在密码学上根本不成立。所以边界划在这里——压缩前
//! 的活儿共享一次，加密及其之后各连接各做。实测这部分很便宜：3.1 MB 的
//! ChaChaPoly 加密约 6 ms，不值得为它另想办法。

use anyhow::{anyhow, Context, Result};
use clipsync_core::SyncMessage;

/// 已编码、已按需压缩的一条消息。
///
/// 用 `Arc` 包起来在多条连接间传递，见 [`crate::transport::NoiseConnection::send_prepared`]。
pub struct PreparedMessage {
    /// 待发字节：压缩后的（`compressed` 为真）或原始 postcard 编码。
    body: Vec<u8>,
    /// 解压还原后的字节数；未压缩时与 `body.len()` 相同。
    plain_len: usize,
    compressed: bool,
}

impl PreparedMessage {
    /// 编码一条消息：postcard 序列化 → 按需压缩。
    ///
    /// 这是广播路径上唯一该做这件事的地方；做完的结果发给多少台设备都只是
    /// 加密与写出的成本。
    pub fn encode(msg: &SyncMessage) -> Result<Self> {
        let plaintext = msg.encode().context("编码消息失败")?;
        let plain_len = plaintext.len();
        let (body, compressed) = pack(msg, plaintext);
        Ok(Self {
            body,
            plain_len,
            compressed,
        })
    }

    /// 实际要写上线的字节数（压缩后的量）。
    pub fn wire_len(&self) -> usize {
        self.body.len()
    }

    /// 压缩前的字节数。
    pub fn plain_len(&self) -> usize {
        self.plain_len
    }

    pub(crate) fn header(&self) -> [u8; Header::LEN] {
        Header::new(self.body.len(), self.plain_len, self.compressed).encode()
    }

    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }
}

/// 消息头帧：告诉接收方要读多少字节、解压后有多长、要不要解压。
///
/// 定长 9 字节，全部大端：线上长度 u32 ‖ 原始长度 u32 ‖ 压缩标记 u8。
///
/// **为什么原始长度也要发**：解压必须有个硬上限，否则一段几 KB 的恶意数据能
/// 解出几 GB。见 [`crate::compress::decompress`]。
pub(crate) struct Header {
    /// 数据帧总字节数（压缩后的量，即实际要从线上读多少）。
    pub(crate) wire_len: usize,
    /// 解压还原后的字节数；未压缩时与 `wire_len` 相同。
    pub(crate) plain_len: usize,
    pub(crate) compressed: bool,
}

impl Header {
    pub(crate) const LEN: usize = 9;

    fn new(wire_len: usize, plain_len: usize, compressed: bool) -> Self {
        Self {
            wire_len,
            plain_len,
            compressed,
        }
    }

    fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&(self.wire_len as u32).to_be_bytes());
        b[4..8].copy_from_slice(&(self.plain_len as u32).to_be_bytes());
        b[8] = self.compressed as u8;
        b
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != Self::LEN {
            return Err(anyhow!(
                "消息头帧长度非法: {}（应为 {}）",
                b.len(),
                Self::LEN
            ));
        }
        Ok(Self {
            wire_len: u32::from_be_bytes(b[0..4].try_into().unwrap()) as usize,
            plain_len: u32::from_be_bytes(b[4..8].try_into().unwrap()) as usize,
            compressed: b[8] != 0,
        })
    }
}

/// 小于此值不压：省下的字节抵不过一次压缩调用的开销。
const COMPRESS_MIN: usize = 16 * 1024;

/// 决定这条消息要不要在帧层压，并给出待发字节。
///
/// **为什么压缩放在帧层**：图片是以裸 RGBA 传的（`ClipContent::Image`），
/// 一张 4K 截图就是 33 MB，而它压完只剩一成上下。在这里压，`ClipContent` 的
/// 类型与内容哈希语义都不用动——哈希始终基于原始字节，回环检测不受影响。
fn pack(msg: &SyncMessage, plaintext: Vec<u8>) -> (Vec<u8>, bool) {
    if plaintext.len() < COMPRESS_MIN {
        return (plaintext, false);
    }
    // 文件分块在应用层已按文件类型自适应压过（见 app 的 `compress` 模块），
    // 这里再压一遍：能压的已经压完，压不动的（jpg/zip）白白多试一次。
    if matches!(msg, SyncMessage::FileChunk { .. }) {
        return (plaintext, false);
    }
    // 先采样探一下。整份试压对压不动的内容是纯浪费——实测能把吞吐砍掉一半。
    if !crate::compress::worth_compressing(&plaintext) {
        return (plaintext, false);
    }
    match crate::compress::compress(&plaintext) {
        // 压完反而更大就退回原始字节。随机/已压缩内容会这样。
        Ok(c) if c.len() < plaintext.len() => {
            // 上层的「已同步 N 字节」报的是**内容**大小，看不出线上实际发了多少。
            // 少了这一行，"压缩到底生效没有"就只能靠猜。
            tracing::debug!(
                "帧层压缩 {} → {} 字节（{:.0}%）",
                plaintext.len(),
                c.len(),
                c.len() as f64 / plaintext.len() as f64 * 100.0
            );
            (c, true)
        }
        _ => (plaintext, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipsync_core::{ClipContent, DeviceId, ImageData};

    fn image_msg(width: u32, height: u32, rgba: Vec<u8>) -> SyncMessage {
        let content = ClipContent::Image(ImageData {
            width,
            height,
            rgba,
        });
        SyncMessage::Clip {
            origin: DeviceId::from_public_key(b"cam"),
            seq: 1,
            content_hash: content.content_hash(),
            content,
        }
    }

    /// 截图状负载：大片同色加少量噪点。
    fn screenshot_like(w: u32, h: u32) -> SyncMessage {
        let mut rgba = vec![0xF0u8; (w * h * 4) as usize];
        for (i, b) in rgba.iter_mut().enumerate() {
            if i % 997 == 0 {
                *b = (i % 251) as u8;
            }
        }
        image_msg(w, h, rgba)
    }

    #[test]
    fn header_roundtrips() {
        let h = Header::new(3_174_747, 33_129_653, true);
        let back = Header::decode(&h.encode()).unwrap();
        assert_eq!(back.wire_len, 3_174_747);
        assert_eq!(back.plain_len, 33_129_653);
        assert!(back.compressed);
    }

    /// 一张截图大小的图片必须在帧层被压掉——这是加压缩的**全部理由**。
    ///
    /// 断言的是压缩比而不是"压过了"：真正要守住的是"4K 截图别再占 33 MB
    /// 带宽"，一个只调用了压缩但压不动的实现同样是失败的。
    #[test]
    fn a_screenshot_sized_image_gets_squeezed() {
        let msg = screenshot_like(3840, 2160);
        let p = PreparedMessage::encode(&msg).unwrap();

        assert!(p.compressed, "截图这么大的图片必须压");
        assert!(
            p.wire_len() * 10 < p.plain_len(),
            "截图应压到一成以下：{} → {}",
            p.plain_len(),
            p.wire_len()
        );
    }

    /// `plain_len` 必须是**压缩前**的长度——接收方拿它当解压的硬上限，
    /// 记成压缩后的值会让每条大消息都解压失败。
    #[test]
    fn plain_len_is_the_uncompressed_size() {
        let msg = screenshot_like(256, 256);
        let p = PreparedMessage::encode(&msg).unwrap();
        assert!(p.compressed, "构造前提：这张图应被压缩");
        assert_eq!(p.plain_len(), msg.encode().unwrap().len());
    }

    /// 压不动的内容退回原始字节，不能因为"压过了"就把更大的结果发出去。
    #[test]
    fn incompressible_content_falls_back_to_raw() {
        // 伪随机 RGBA，模拟照片/噪点——deflate 只会让它变大。
        let mut x: u32 = 0x1234_5678;
        let rgba: Vec<u8> = std::iter::repeat_with(|| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 16) as u8
        })
        .take(512 * 1024)
        .collect();
        let msg = image_msg(512, 256, rgba);
        let p = PreparedMessage::encode(&msg).unwrap();
        assert!(!p.compressed, "随机内容压不动，应退回原始字节");
        assert_eq!(p.wire_len(), p.plain_len());
    }

    /// 小消息不压，且 `plain_len == wire_len`。
    #[test]
    fn small_message_is_left_alone() {
        let p = PreparedMessage::encode(&SyncMessage::Ping).unwrap();
        assert!(!p.compressed);
        assert_eq!(p.wire_len(), p.plain_len());
    }

    /// 同一条消息编码两次必须字节完全相同——这是"压一次发给 N 台"能成立的
    /// 前提。真出现不确定性（比如将来换个带随机数的压缩器），共享就会让某些
    /// 对端收到与头帧声明不符的数据。
    #[test]
    fn encoding_is_deterministic() {
        let msg = screenshot_like(256, 128);
        let a = PreparedMessage::encode(&msg).unwrap();
        let b = PreparedMessage::encode(&msg).unwrap();
        assert_eq!(a.body(), b.body());
        assert_eq!(a.header(), b.header());
    }

    /// 文件分块不在帧层重压：应用层已按文件类型压过。
    #[test]
    fn file_chunk_skips_frame_compression() {
        let msg = SyncMessage::FileChunk {
            generation: 1,
            file_id: 42,
            offset: 0,
            data: vec![0u8; 64 * 1024],
            compressed: false,
            plain_len: 64 * 1024,
        };
        let p = PreparedMessage::encode(&msg).unwrap();
        assert!(!p.compressed, "文件分块不该在帧层再压一遍");
    }
}
