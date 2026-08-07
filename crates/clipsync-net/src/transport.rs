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
//! 大消息（图片/文件）由 `send` 内部按 Noise 单帧上限自动切分为多帧，
//! `recv` 侧重组，对上层透明。

use std::net::TcpStream;

use anyhow::{anyhow, Context, Result};
use clipsync_core::SyncMessage;

use crate::crypto::NOISE_PARAMS;
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
    pub fn connect(
        stream: TcpStream,
        local_private: &[u8],
        remote_public: &[u8],
    ) -> Result<Self> {
        tune_socket(&stream);
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
        tune_socket(&stream);
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

    /// 发送一条消息：编码 → 按帧上限切分 → 逐帧加密写出。
    ///
    /// 帧结构：先发一个头帧（4 字节大端的明文总长度），再发若干密文数据帧。
    /// 接收方据总长度重组。
    pub fn send(&mut self, msg: &SyncMessage) -> Result<()> {
        let plaintext = msg.encode().context("编码消息失败")?;

        // 头帧：明文总长度（便于接收方预分配与判断重组完成）。
        let mut header = Vec::with_capacity(4);
        header.extend_from_slice(&(plaintext.len() as u32).to_be_bytes());
        self.send_encrypted(&header)?;

        // 数据帧：按 Noise 明文上限切分。
        for chunk in plaintext.chunks(MAX_FRAME_PAYLOAD) {
            self.send_encrypted(chunk)?;
        }
        Ok(())
    }

    /// 接收一条消息，重组所有数据帧。对端关闭返回 `Ok(None)`。
    pub fn recv(&mut self) -> Result<Option<SyncMessage>> {
        // 读头帧得到明文总长度。
        let header = match self.recv_encrypted()? {
            Some(h) => h,
            None => return Ok(None),
        };
        if header.len() != 4 {
            return Err(anyhow!("消息头帧长度非法: {}", header.len()));
        }
        let total = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let msg = self.recv_body(total)?;
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
                // 头帧是加密的（16 字节 tag + 4 字节明文长度）。先解密它。
                let mut hdr_plain = vec![0u8; header.len()];
                let n = self
                    .noise
                    .read_message(&header, &mut hdr_plain)
                    .context("解密消息头帧失败")?;
                hdr_plain.truncate(n);
                if hdr_plain.len() != 4 {
                    return Err(anyhow!("消息头帧明文长度非法: {}", hdr_plain.len()));
                }
                let total =
                    u32::from_be_bytes([hdr_plain[0], hdr_plain[1], hdr_plain[2], hdr_plain[3]])
                        as usize;

                // 已进入消息中段：取消超时，阻塞读完所有数据帧。
                let prev = self.stream.read_timeout().ok().flatten();
                self.stream.set_read_timeout(None).ok();
                let result = self.recv_body(total);
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

    /// 读取并重组消息体（不含头帧），头帧已给出明文总长度 `total`。
    fn recv_body(&mut self, total: usize) -> Result<SyncMessage> {
        // 按声明长度**预留上限**而非照单全收：`total` 来自对端头帧，虽已由
        // Noise 认证（只有已配对设备发得出），但一个 u32 就能声明 4 GB。
        // 对端实现出错或内容异常时，照着分配会当场把内存打爆，而实际数据
        // 可能一个字节都没到。这里只先要一小块，随实际收到的数据自然增长，
        // 下面的 `> total` 检查负责拦住真正超长的消息。
        const PREALLOC_CAP: usize = 1024 * 1024;
        let mut plaintext = Vec::with_capacity(total.min(PREALLOC_CAP));
        while plaintext.len() < total {
            let chunk = self
                .recv_encrypted()?
                .ok_or_else(|| anyhow!("重组消息时连接中断"))?;
            plaintext.extend_from_slice(&chunk);
            if plaintext.len() > total {
                return Err(anyhow!("重组消息超出声明长度"));
            }
        }
        SyncMessage::decode(&plaintext).context("解码消息失败")
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

/// 调整套接字参数以适配本协议的收发模式。
///
/// **关闭 Nagle 算法**：本协议是"一帧接一帧"的请求/流式模式，Nagle 会为了合并
/// 小包而等待对端 ACK；与接收端的延迟确认叠加时，可能造成数十毫秒的停顿——
/// 对文件传输吞吐和剪贴板同步延迟都是明显损害。我们自己已经把长度前缀与负载
/// 合并成单次写出，不需要内核再代为合并。
///
/// 设置失败不致命（个别平台/虚拟网卡可能不支持），仅记录后继续。
fn tune_socket(stream: &TcpStream) {
    if let Err(e) = stream.set_nodelay(true) {
        tracing::debug!("设置 TCP_NODELAY 失败（不影响功能，可能略增延迟）: {e}");
    }
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
mod tests {
    use super::*;
    use crate::crypto::StaticIdentity;
    use clipsync_core::{ClipContent, DeviceId, SyncMessage};
    use std::net::{TcpListener, TcpStream};

    /// 端到端：真实 socket 上完成 Noise_IK 握手并双向收发加密消息，
    /// 覆盖握手、头帧加密、多帧重组、身份认证（remote_static）。
    #[test]
    fn noise_roundtrip_over_tcp() {
        let server_id = StaticIdentity::generate().unwrap();
        let client_id = StaticIdentity::generate().unwrap();
        let server_pub = server_id.public_key.clone();
        let client_pub = client_id.public_key.clone();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // 服务端：响应方。
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut conn = NoiseConnection::accept(s, &server_id.private_key).unwrap();
            // 认证：应看到客户端静态公钥。
            assert_eq!(conn.remote_static().unwrap(), client_pub);
            // 收一条文本，回一条较大的图片（触发多帧）。
            let got = conn.recv().unwrap().unwrap();
            match got {
                SyncMessage::Clip { content, .. } => {
                    assert_eq!(content, ClipContent::Text("hello".into()));
                }
                _ => panic!("期望 Clip"),
            }
            let big = ClipContent::Image(clipsync_core::ImageData {
                width: 100,
                height: 200,
                rgba: vec![7u8; 100 * 200 * 4], // 80000 字节，跨多帧
            });
            conn.send(&SyncMessage::Clip {
                origin: DeviceId::from_public_key(b"srv"),
                seq: 1,
                content_hash: big.content_hash(),
                content: big,
            })
            .unwrap();
        });

        // 客户端：发起方，已知服务端公钥（IK）。
        let stream = TcpStream::connect(addr).unwrap();
        let mut conn = NoiseConnection::connect(stream, &client_id.private_key, &server_pub).unwrap();
        assert_eq!(conn.remote_static().unwrap(), server_pub);

        conn.send(&SyncMessage::Clip {
            origin: DeviceId::from_public_key(b"cli"),
            seq: 0,
            content_hash: 0,
            content: ClipContent::Text("hello".into()),
        })
        .unwrap();

        // 收多帧大图片并校验完整重组。
        let reply = conn.recv().unwrap().unwrap();
        match reply {
            SyncMessage::Clip { content, .. } => match content {
                ClipContent::Image(img) => {
                    assert_eq!(img.width, 100);
                    assert_eq!(img.height, 200);
                    assert_eq!(img.rgba.len(), 100 * 200 * 4);
                    assert!(img.rgba.iter().all(|&b| b == 7));
                }
                _ => panic!("期望 Image"),
            },
            _ => panic!("期望 Clip"),
        }

        server.join().unwrap();
    }

    /// 用错误的对端公钥发起 IK 握手应失败（认证保护）。
    #[test]
    fn handshake_fails_with_wrong_remote_key() {
        let server_id = StaticIdentity::generate().unwrap();
        let client_id = StaticIdentity::generate().unwrap();
        let wrong_pub = StaticIdentity::generate().unwrap().public_key;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            if let Ok((s, _)) = listener.accept() {
                // 响应方握手会因发起方用错公钥而失败；忽略结果。
                let _ = NoiseConnection::accept(s, &server_id.private_key);
            }
        });

        let stream = TcpStream::connect(addr).unwrap();
        // 用错误的服务端公钥 → 握手应失败。
        let result = NoiseConnection::connect(stream, &client_id.private_key, &wrong_pub);
        assert!(result.is_err(), "用错误对端公钥握手不应成功");

        let _ = server.join();
    }
}
