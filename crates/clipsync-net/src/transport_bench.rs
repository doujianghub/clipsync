//! 传输层吞吐基线（手动跑，不进 CI）。
//!
//! 存在的意义是把「链路慢」和「代码慢」分开。用户报告传输慢时，先在本机
//! 跑一遍：数字仍在百 MB/s 量级，就说明瓶颈不在加密、分帧或 syscall 上，
//! 该去量网络而不是改代码。
//!
//! **负载必须是不可压缩的**。帧层会自适应压缩，拿全同色的字节来跑，量到的
//! 是压缩比而不是吞吐——数字会好看得离谱，然后被当成"传输很快"信以为真。

use super::*;
use crate::crypto::StaticIdentity;
use clipsync_core::{ClipContent, DeviceId, SyncMessage};
use std::net::{TcpListener, TcpStream};
use std::time::Instant;

/// 手动基线：loopback 上跑 200 MB，量出「加密 + 分帧 + syscall」的上限。
///
/// 跑法：`cargo test -p clipsync-net --release -- --ignored --nocapture throughput`
#[test]
#[ignore = "耗时的性能基线，手动跑"]
fn throughput_baseline() {
    const CHUNK: usize = 256 * 1024;
    const ROUNDS: usize = 800; // 200 MB

    let srv_id = StaticIdentity::generate().unwrap();
    let cli_id = StaticIdentity::generate().unwrap();
    let srv_pub = srv_id.public_key.clone();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = std::thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        let mut conn = NoiseConnection::accept(s, &srv_id.private_key).unwrap();
        let t0 = Instant::now();
        let mut got = 0usize;
        for _ in 0..ROUNDS {
            match conn.recv().unwrap().unwrap() {
                SyncMessage::Clip { content, .. } => {
                    if let ClipContent::Image(i) = content {
                        got += i.rgba.len();
                    }
                }
                _ => panic!(),
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        println!(
            "接收 {:.1} MB，用时 {:.2}s → {:.1} MB/s",
            got as f64 / 1e6,
            secs,
            got as f64 / 1e6 / secs
        );
    });

    let stream = TcpStream::connect(addr).unwrap();
    let mut conn = NoiseConnection::connect(stream, &cli_id.private_key, &srv_pub).unwrap();
    // 伪随机负载：压不动，于是走的是原始字节路径，量到的才是真实吞吐。
    let mut x: u32 = 0x1234_5678;
    let payload: Vec<u8> = std::iter::repeat_with(|| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (x >> 16) as u8
    })
    .take(CHUNK)
    .collect();
    let t0 = Instant::now();
    for i in 0..ROUNDS {
        let img = ClipContent::Image(clipsync_core::ImageData {
            width: 1,
            height: 1,
            rgba: payload.clone(),
        });
        conn.send(&SyncMessage::Clip {
            origin: DeviceId::from_public_key(b"cli"),
            seq: i as u64,
            content_hash: 0,
            content: img,
        })
        .unwrap();
    }
    let secs = t0.elapsed().as_secs_f64();
    println!(
        "发送 {:.1} MB，用时 {:.2}s → {:.1} MB/s",
        (CHUNK * ROUNDS) as f64 / 1e6,
        secs,
        (CHUNK * ROUNDS) as f64 / 1e6 / secs
    );
    server.join().unwrap();
}
