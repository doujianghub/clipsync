//! `transport` 的单元测试（单独成文件以控制行数，仍是其子模块）。
//!
//! 只测**连接**这一层：握手、认证、多帧重组、端到端还原。压缩该不该发生、
//! 压到多少，是 `prepared` 的事，测试也在那边。

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

/// 一份 [`PreparedMessage`] 发给**两条独立连接**，两边都必须逐字节还原。
///
/// 这是"压一次发给 N 台"能成立的全部前提。共享的是压缩后的明文帧，而每条
/// Noise 连接有各自的会话密钥与 nonce 序列——加密仍是各做各的。真要是哪天
/// 把共享的边界画错、连密文一起复用了，两端的 nonce 就会错位，这个测试会
/// 当场失败，而不是等到线上出现"某台设备收不到图片"。
#[test]
fn one_prepared_message_serves_two_connections() {
    let msg = screenshot_like(640, 480);
    let expect = msg.clone();
    let prepared = PreparedMessage::encode(&msg).unwrap();
    assert!(
        prepared.wire_len() < prepared.plain_len(),
        "构造前提：应被压缩"
    );

    let mut servers = Vec::new();
    let mut conns = Vec::new();
    for _ in 0..2 {
        let srv = StaticIdentity::generate().unwrap();
        let cli = StaticIdentity::generate().unwrap();
        let srv_pub = srv.public_key.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        servers.push(std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut conn = NoiseConnection::accept(s, &srv.private_key).unwrap();
            conn.recv().unwrap().unwrap()
        }));
        let stream = TcpStream::connect(addr).unwrap();
        conns.push(NoiseConnection::connect(stream, &cli.private_key, &srv_pub).unwrap());
    }

    // 同一份字节，发两次。
    for conn in conns.iter_mut() {
        conn.send_prepared(&prepared).unwrap();
    }
    for (i, s) in servers.into_iter().enumerate() {
        assert_eq!(s.join().unwrap(), expect, "第 {i} 条连接的还原结果不一致");
    }
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
