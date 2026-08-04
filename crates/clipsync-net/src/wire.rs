//! 长度前缀分块帧的同步读写。
//!
//! Noise 加密流传输的是一个个"帧"：每帧前缀一个 4 字节大端长度，随后是该
//! 长度的负载字节。这样接收方能在字节流上准确切分出每条消息 / 每个密文块。
//!
//! Noise 单条消息密文上限为 65535 字节，因此更大的负载（如图片、文件）在
//! 上层按块切分后逐帧发送；本模块只负责单帧的可靠读写，不关心内容语义。

use std::io::{self, Read, Write};

/// 单帧最大负载字节数。
///
/// Noise transport 消息含 16 字节 AEAD tag，明文上限 65535-16。这里限制明文
/// 分块不超过 64000，留足余量；上层据此切分大负载。
pub const MAX_FRAME_PAYLOAD: usize = 64_000;

/// 帧长度上限（含加密开销的保守上界），用于读取端防御异常/恶意长度。
const MAX_FRAME_ON_WIRE: u32 = 65_535;

/// 写入一帧：4 字节大端长度前缀 + 负载。
///
/// **一次性写出**：长度前缀与负载合并到同一次 `write_all`。分两次写会形成
/// "小包紧跟大包"的模式，在启用 Nagle 算法的连接上可能让 4 字节前缀滞留在
/// 内核缓冲里等待对端 ACK，与延迟确认叠加后造成数十毫秒的停顿。
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME_ON_WIRE as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "帧负载超过线缆上限",
        ));
    }
    let mut framed = Vec::with_capacity(4 + payload.len());
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    framed.extend_from_slice(payload);
    w.write_all(&framed)?;
    w.flush()
}

/// 读取一帧，返回负载字节。对端正常关闭（读长度时即 EOF）返回 `Ok(None)`。
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    // 读长度前缀：若在此处遇到 EOF 且一字节未读，视为对端正常关闭。
    match read_exact_or_eof(r, &mut len_buf)? {
        ReadEnd::Eof => return Ok(None),
        ReadEnd::Full => {}
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_ON_WIRE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "帧长度超过上限（可能为异常数据）",
        ));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok(Some(payload))
}

enum ReadEnd {
    Full,
    Eof,
}

/// 读满 `buf`；若一字节都没读到就 EOF 返回 `Eof`（正常关闭），
/// 若读了一部分才 EOF 则是 `UnexpectedEof` 错误（半个帧，异常）。
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<ReadEnd> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(ReadEnd::Eof);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "读取帧长度前缀时连接中断",
                ));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(ReadEnd::Full)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip_single_frame() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello").unwrap();
        let mut cur = Cursor::new(buf);
        assert_eq!(read_frame(&mut cur).unwrap().unwrap(), b"hello");
    }

    #[test]
    fn roundtrip_multiple_frames() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"first").unwrap();
        write_frame(&mut buf, b"second").unwrap();
        write_frame(&mut buf, b"").unwrap(); // 空帧也应可读
        let mut cur = Cursor::new(buf);
        assert_eq!(read_frame(&mut cur).unwrap().unwrap(), b"first");
        assert_eq!(read_frame(&mut cur).unwrap().unwrap(), b"second");
        assert_eq!(read_frame(&mut cur).unwrap().unwrap(), b"");
    }

    #[test]
    fn clean_eof_returns_none() {
        let mut cur = Cursor::new(Vec::new());
        assert!(read_frame(&mut cur).unwrap().is_none());
    }

    #[test]
    fn truncated_length_prefix_is_error() {
        // 只有 2 字节，不足 4 字节长度前缀 → UnexpectedEof。
        let mut cur = Cursor::new(vec![0u8, 1]);
        let err = read_frame(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn truncated_payload_is_error() {
        // 声明长度 10，但只给 3 字节负载。
        let mut buf = 10u32.to_be_bytes().to_vec();
        buf.extend_from_slice(b"abc");
        let mut cur = Cursor::new(buf);
        assert!(read_frame(&mut cur).is_err());
    }
}
