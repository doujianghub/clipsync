//! `transport` 的单元测试（单独成文件以控制行数，仍是其子模块）。

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

/// 一张截图大小的图片必须在帧层被压掉——这是加压缩的**全部理由**。
///
/// 断言的是压缩比而不是"压过了"：真正要守住的是"4K 截图别再占 33 MB 带宽"，
/// 一个只调用了压缩但压不动的实现同样是失败的。
#[test]
fn a_screenshot_sized_image_gets_squeezed() {
    let img = screenshot_like(3840, 2160);
    let raw = img.encode().unwrap();
    let (body, compressed) = super::pack(&img, &raw);

    assert!(compressed, "截图这么大的图片必须压");
    assert!(
        body.len() * 10 < raw.len(),
        "截图应压到一成以下：{} → {}",
        raw.len(),
        body.len()
    );
}

/// 文件分块不该在帧层再压一遍——应用层已按文件类型自适应压过了。
#[test]
fn file_chunks_are_left_alone() {
    let msg = SyncMessage::FileChunk {
        generation: 1,
        file_id: 2,
        offset: 0,
        data: vec![0u8; 256 * 1024],
        compressed: true,
        plain_len: 256 * 1024,
    };
    let raw = msg.encode().unwrap();
    let (body, compressed) = super::pack(&msg, &raw);
    assert!(!compressed, "文件分块应原样透传，避免重复压缩");
    assert_eq!(body.len(), raw.len());
}

/// 小消息不值得压：省下的字节抵不过一次压缩调用。
#[test]
fn small_messages_skip_compression() {
    let msg = SyncMessage::Clip {
        origin: DeviceId::from_public_key(b"x"),
        seq: 0,
        content_hash: 0,
        content: ClipContent::Text("短文本".into()),
    };
    let raw = msg.encode().unwrap();
    let (_, compressed) = super::pack(&msg, &raw);
    assert!(!compressed);
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
    let raw = msg.encode().unwrap();
    let (body, compressed) = super::pack(&msg, &raw);
    assert!(!compressed, "随机内容压不动，应退回原始字节");
    assert_eq!(body.len(), raw.len());
}

/// 端到端：一张大图片过真实 socket 走一遭，字节必须**逐字节还原**。
///
/// 压缩最危险的失败模式不是报错，而是悄悄还原出不一样的东西——图片会显示
/// 成花屏，而日志里一切正常。所以这里比对的是完整像素而不是长度。
#[test]
fn a_compressed_image_survives_a_real_roundtrip() {
    let srv = StaticIdentity::generate().unwrap();
    let cli = StaticIdentity::generate().unwrap();
    let srv_pub = srv.public_key.clone();
    let sent = screenshot_like(1280, 720);
    let expect = sent.clone();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        let mut conn = NoiseConnection::accept(s, &srv.private_key).unwrap();
        conn.recv().unwrap().unwrap()
    });

    let stream = TcpStream::connect(addr).unwrap();
    let mut conn = NoiseConnection::connect(stream, &cli.private_key, &srv_pub).unwrap();
    conn.send(&sent).unwrap();

    assert_eq!(server.join().unwrap(), expect, "解压后必须逐字节一致");
}

/// 构造一条"截图那样"的图片消息：大片同色 + 少量噪点。
fn screenshot_like(w: u32, h: u32) -> SyncMessage {
    let mut rgba = vec![0xF0u8; (w * h * 4) as usize];
    for (i, b) in rgba.iter_mut().enumerate() {
        if i % 997 == 0 {
            *b = (i % 251) as u8;
        }
    }
    image_msg(w, h, rgba)
}

fn image_msg(width: u32, height: u32, rgba: Vec<u8>) -> SyncMessage {
    let content = ClipContent::Image(clipsync_core::ImageData {
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
