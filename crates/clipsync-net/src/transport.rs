//! 加密传输：TCP + Noise_IK 握手 + 长度前缀分块帧。
//!
//! 采用同步阻塞 IO + 每连接一线程模型（而非 async 运行时），契合"个人少量
//! 设备"的场景与"低占用、稳定简便"目标：snow 是同步 API，与阻塞 TCP 天然
//! 契合，省去 async 桥接复杂度与重型运行时开销。
//!
//! 连接生命周期：
//!   1. TCP 建连（发起方 connect / 响应方 accept）。
//!   2. Noise_IK 握手：发起方须已知响应方静态公钥（配对后成立）。
//!   3. 进入 transport 模式，双向收发加密的 `SyncMessage`（postcard 编码）。
//!
//! 大消息（图片/文件）由 `send` 内部**按需压缩**后再按 Noise 单帧上限切分为
//! 多帧，`recv` 侧重组还原，对上层透明——`SyncMessage` 与内容哈希都不受影响。
//!
//! 压缩放在这一层是因为图片以裸 RGBA 传输（`ClipContent::Image`），一张 4K
//! 截图就是 33 MB 而压完只剩 6%。放在这里，上层的类型与哈希语义都不用动。

use std::net::TcpStream;

use anyhow::{anyhow, Context, Result};
use clipsync_core::SyncMessage;

use crate::crypto::NOISE_PARAMS;
use crate::prepared::{Header, PreparedMessage};
use crate::wire::{read_frame, write_frame, MAX_FRAME_PAYLOAD};

/// 一条已完成握手的加密连接。
pub struct NoiseConnection {
    stream: TcpStream,
    noise: snow::TransportState,
    /// 复用的加密输出缓冲，避免每帧分配。
    enc_buf: Vec<u8>,
}

impl NoiseConnection {
    /// 作为发起方建立连接：TCP connect + Noise_IK 握手。
    ///
    /// `local_private` 为本机静态私钥，`remote_public` 为对端静态公钥
    /// （来自配对记录）。IK 模式下发起方须预置对端公钥。
    pub fn connect(stream: TcpStream, local_private: &[u8], remote_public: &[u8]) -> Result<Self> {
        crate::sockopt::tune(&stream);
        let handshake = snow::Builder::new(NOISE_PARAMS.parse()?)
            .local_private_key(local_private)
            .remote_public_key(remote_public)
            .build_initiator()
            .context("构建 Noise 发起方失败")?;
        let noise = run_handshake(stream.try_clone()?, handshake, Role::Initiator)?;
        Ok(Self {
            stream,
            noise,
            enc_buf: vec![0u8; 65_535],
        })
    }

    /// 作为响应方接受连接：Noise_IK 握手。
    ///
    /// 响应方在握手中获得发起方静态公钥，返回时通过 [`Self::remote_static`]
    /// 暴露，供上层比对配对记录（认证对端身份）。
    pub fn accept(stream: TcpStream, local_private: &[u8]) -> Result<Self> {
        crate::sockopt::tune(&stream);
        let handshake = snow::Builder::new(NOISE_PARAMS.parse()?)
            .local_private_key(local_private)
            .build_responder()
            .context("构建 Noise 响应方失败")?;
        let noise = run_handshake(stream.try_clone()?, handshake, Role::Responder)?;
        Ok(Self {
            stream,
            noise,
            enc_buf: vec![0u8; 65_535],
        })
    }

    /// 对端静态公钥（IK 握手后可得），用于身份认证。
    pub fn remote_static(&self) -> Option<Vec<u8>> {
        self.noise.get_remote_static().map(|k| k.to_vec())
    }

    /// 发送一条消息：编码 → 按需压缩 → 按帧上限切分 → 逐帧加密写出。
    ///
    /// 单播用这个。**广播请改用 [`PreparedMessage::encode`] 一次、再对每条
    /// 连接调 [`Self::send_prepared`]**，否则每条连接会把同一份内容各压一遍。
    pub fn send(&mut self, msg: &SyncMessage) -> Result<()> {
        self.send_prepared(&PreparedMessage::encode(msg)?)
    }

    /// 发送一条已编码压缩的消息。同一个 [`PreparedMessage`] 可发给任意多条连接。
    ///
    /// 帧结构：先发一个头帧（见 `Header`），再发若干密文数据帧。
    ///
    /// **这里的耗时几乎全是等内核收数据**：加密本身很便宜（实测 3.1 MB 约
    /// 6 ms），而套接字被 `sockopt::tune` 设了 128 KiB 的未发送水位，大消息
    /// 会在这里被 `write` 反复挡住。所以这个打点量的其实是「本机把字节交给
    /// 内核花了多久」——它和接收侧的「读消息体」一起，把网络那段夹在中间。
    pub fn send_prepared(&mut self, prepared: &PreparedMessage) -> Result<()> {
        let t0 = std::time::Instant::now();
        self.send_encrypted(&prepared.header())?;

        // 数据帧：按 Noise 明文上限切分。
        for chunk in prepared.body().chunks(MAX_FRAME_PAYLOAD) {
            self.send_encrypted(chunk)?;
        }
        clipsync_core::timing::note_slow_bytes("消息上线", prepared.wire_len(), t0);
        Ok(())
    }

    /// 接收一条消息，重组所有数据帧。对端关闭返回 `Ok(None)`。
    pub fn recv(&mut self) -> Result<Option<SyncMessage>> {
        let header = match self.recv_encrypted()? {
            Some(h) => h,
            None => return Ok(None),
        };
        let msg = self.recv_message(Header::decode(&header)?)?;
        Ok(Some(msg))
    }

    /// 加密一段明文并作为一帧写出。
    fn send_encrypted(&mut self, plaintext: &[u8]) -> Result<()> {
        let n = self
            .noise
            .write_message(plaintext, &mut self.enc_buf)
            .context("Noise 加密失败")?;
        write_frame(&mut self.stream, &self.enc_buf[..n]).context("写入加密帧失败")?;
        Ok(())
    }

    /// 读一帧并解密。对端关闭返回 `Ok(None)`。
    fn recv_encrypted(&mut self) -> Result<Option<Vec<u8>>> {
        let frame = match read_frame(&mut self.stream).context("读取加密帧失败")? {
            Some(f) => f,
            None => return Ok(None),
        };
        let mut out = vec![0u8; frame.len()];
        let n = self
            .noise
            .read_message(&frame, &mut out)
            .context("Noise 解密失败")?;
        out.truncate(n);
        Ok(Some(out))
    }

    /// 设置底层 TCP 读超时。配合 [`Self::recv_timeout`] 实现单线程双向收发。
    pub fn set_read_timeout(&mut self, timeout: Option<std::time::Duration>) -> Result<()> {
        self.stream
            .set_read_timeout(timeout)
            .context("设置读超时失败")
    }

    /// 带超时的接收：
    ///   - `Message(msg)`：收到一条完整消息。
    ///   - `Closed`：对端正常关闭。
    ///   - `Timeout`：本轮超时内无新消息（用于让出 CPU 回到发送检查）。
    ///
    /// 超时仅在"消息尚未开始"时返回；一旦读到消息首字节，会临时取消超时把整条
    /// 消息读完，避免在多帧消息中途因超时切断导致流错位。
    pub fn recv_timeout(&mut self) -> Result<RecvOutcome> {
        // 先以当前（带超时）设置尝试读头帧。
        match read_frame(&mut self.stream) {
            Ok(Some(header)) => {
                if header.len() < 16 {
                    return Err(anyhow!("消息头帧过短（疑似损坏）"));
                }
                // 头帧是加密的（16 字节 tag + 头部本身）。先解密它。
                let mut hdr_plain = vec![0u8; header.len()];
                let n = self
                    .noise
                    .read_message(&header, &mut hdr_plain)
                    .context("解密消息头帧失败")?;
                hdr_plain.truncate(n);
                let hdr = Header::decode(&hdr_plain)?;

                // 已进入消息中段：取消超时，阻塞读完所有数据帧。
                let prev = self.stream.read_timeout().ok().flatten();
                self.stream.set_read_timeout(None).ok();
                let result = self.recv_message(hdr);
                // 恢复超时设置。
                self.stream.set_read_timeout(prev).ok();
                let msg = result?;
                Ok(RecvOutcome::Message(msg))
            }
            Ok(None) => Ok(RecvOutcome::Closed),
            Err(e) if is_timeout(&e) => Ok(RecvOutcome::Timeout),
            Err(e) => Err(anyhow::Error::from(e)).context("读取消息头帧失败"),
        }
    }

    /// 读齐消息体、按需解压、解码。
    ///
    /// 分两段打点，因为它们的成因完全不同：**读体**慢是链路或对端发送慢，
    /// **解压解码**慢是本机 CPU 不够。混成一个数字就又要靠猜了。
    fn recv_message(&mut self, hdr: Header) -> Result<SyncMessage> {
        let t0 = std::time::Instant::now();
        let body = self.recv_body(hdr.wire_len)?;
        clipsync_core::timing::note_slow_bytes("读取消息体", hdr.wire_len, t0);

        let t1 = std::time::Instant::now();
        let plaintext = if hdr.compressed {
            std::borrow::Cow::Owned(crate::compress::decompress(&body, hdr.plain_len)?)
        } else {
            std::borrow::Cow::Borrowed(&body[..])
        };
        let msg = SyncMessage::decode(&plaintext).context("解码消息失败")?;
        clipsync_core::timing::note_slow_bytes("解压与解码", hdr.plain_len, t1);
        Ok(msg)
    }

    /// 读取并重组消息体（不含头帧），共 `total` 字节。
    fn recv_body(&mut self, total: usize) -> Result<Vec<u8>> {
        // 按声明长度**预留上限**而非照单全收：`total` 来自对端头帧，虽已由
        // Noise 认证（只有已配对设备发得出），但一个 u32 就能声明 4 GB。
        // 对端实现出错或内容异常时，照着分配会当场把内存打爆，而实际数据
        // 可能一个字节都没到。这里只先要一小块，随实际收到的数据自然增长，
        // 下面的 `> total` 检查负责拦住真正超长的消息。
        const PREALLOC_CAP: usize = 1024 * 1024;
        let mut body = Vec::with_capacity(total.min(PREALLOC_CAP));
        while body.len() < total {
            let chunk = self
                .recv_encrypted()?
                .ok_or_else(|| anyhow!("重组消息时连接中断"))?;
            body.extend_from_slice(&chunk);
            if body.len() > total {
                return Err(anyhow!("重组消息超出声明长度"));
            }
        }
        Ok(body)
    }
}

/// [`NoiseConnection::recv_timeout`] 的结果。
pub enum RecvOutcome {
    Message(SyncMessage),
    Closed,
    Timeout,
}

/// 判断 IO 错误是否为读超时（socket read timeout 在各平台表现为
/// `WouldBlock` 或 `TimedOut`）。
fn is_timeout(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

enum Role {
    Initiator,
    Responder,
}

/// 执行 Noise_IK 握手（两条消息：-> e, es, s, ss ; <- e, ee, se），完成后转入
/// transport 模式。`io` 为 TCP 流的独立句柄（try_clone）。握手状态按值传入，
/// 完成后消耗自身转为 transport 状态（snow 的 `into_transport_mode` 要求 owned）。
fn run_handshake(
    mut io: TcpStream,
    mut handshake: snow::HandshakeState,
    role: Role,
) -> Result<snow::TransportState> {
    let mut buf = vec![0u8; 65_535];

    match role {
        Role::Initiator => {
            // -> 第一条：发起方先写。
            let n = handshake
                .write_message(&[], &mut buf)
                .context("握手写消息1失败")?;
            write_frame(&mut io, &buf[..n]).context("发送握手消息1失败")?;

            // <- 第二条：读响应方。
            let frame = read_frame(&mut io)
                .context("读取握手消息2失败")?
                .ok_or_else(|| anyhow!("握手期间对端关闭"))?;
            handshake
                .read_message(&frame, &mut buf)
                .context("处理握手消息2失败")?;
        }
        Role::Responder => {
            // <- 第一条：读发起方。
            let frame = read_frame(&mut io)
                .context("读取握手消息1失败")?
                .ok_or_else(|| anyhow!("握手期间对端关闭"))?;
            handshake
                .read_message(&frame, &mut buf)
                .context("处理握手消息1失败")?;

            // -> 第二条：写回发起方。
            let n = handshake
                .write_message(&[], &mut buf)
                .context("握手写消息2失败")?;
            write_frame(&mut io, &buf[..n]).context("发送握手消息2失败")?;
        }
    }

    if !handshake.is_handshake_finished() {
        return Err(anyhow!("Noise 握手未完成"));
    }
    let transport = handshake
        .into_transport_mode()
        .context("转入 Noise transport 模式失败")?;
    Ok(transport)
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "transport_bench.rs"]
mod bench;
