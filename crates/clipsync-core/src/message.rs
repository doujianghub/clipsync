//! 设备间传输的应用层消息。
//!
//! 传输编解码用 `postcard`（紧凑二进制）。这些消息在 Noise 加密流
//! 内传输（见 `clipsync-net`），因此本身不含认证字段。

use serde::{Deserialize, Serialize};

use crate::content::ClipContent;
use crate::device::DeviceId;

/// 协议版本，握手后校验，避免不兼容版本互联出错。
pub const PROTOCOL_VERSION: u16 = 1;

/// 应用层消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMessage {
    /// 连接建立后的第一条消息，交换基本信息。
    Hello {
        protocol: u16,
        device: DeviceId,
        device_name: String,
    },
    /// 一次剪贴板更新广播。
    Clip {
        /// 源设备（防回环/展示用）。
        origin: DeviceId,
        /// 单调递增序号（源设备内唯一），配合内容哈希做去重与冲突判定。
        seq: u64,
        /// 内容确定性哈希（对端可先比对，相同则跳过写入）。
        content_hash: u64,
        content: ClipContent,
    },
    /// 通告本机当前可达地址，供对端更新地址簿。
    ///
    /// 这是**通用的跨网络路径发现机制**：不依赖任何特定组网产品的 API，只要
    /// 双方能通过任意一条路径连通，就能把各自的全部地址（物理网卡、
    /// Tailscale/ZeroTier/WireGuard 等覆盖网、公网）同步给对方。这样当某条
    /// 路径失效时，对端仍握有其它候选地址可尝试。
    ///
    /// 地址的优先级由**接收方**依据自身网络位置判定（同一个 IP 对不同设备可能
    /// 是"同网段直连"或"跨网覆盖"），故此处只传裸地址。
    Addresses { addrs: Vec<std::net::SocketAddr> },

    // ———————————————————————————————————————————————————————————
    // 文件内容传输
    //
    // 文件内容不随 `Clip` 一起发送，而是走下面这组消息独立搬运。这样做换来
    // 三个关键性质：
    //   1. **可取代**：分块之间可检查代际号，新剪贴板��容能立即中止旧传输。
    //   2. **不阻塞**：块与块之间可插入文本同步，传大文件时文本仍秒达。
    //   3. **可续传**：接收方按 `file_id` 缓存已收字节，重复内容可断点续传
    //      甚至零传输命中。
    // ———————————————————————————————————————————————————————————
    /// 接收方请求某文件从 `offset` 开始的内容。
    ///
    /// `offset` 为接收方已持有的字节数——这正是断点续传的实现：缓存里有多少
    /// 就从多少之后要。若缓存已完整则根本不会发出本消息。
    FileNeed {
        /// 对应 `Clip` 的 seq，用于识别该请求属于哪一次剪贴板内容。
        generation: u64,
        file_id: u64,
        offset: u64,
    },
    /// 一个文件数据块。
    FileChunk {
        generation: u64,
        file_id: u64,
        offset: u64,
        data: Vec<u8>,
    },
    /// 某文件全部内容已发送完毕，附内容校验哈希。
    ///
    /// 接收方据此校验完整性；不匹配则丢弃缓存重新索取，绝不产生损坏文件。
    FileDone {
        generation: u64,
        file_id: u64,
        content_hash: u64,
    },
    /// 发送方放弃某代际的文件传输（通常因为剪贴板已更新为新内容）。
    ///
    /// 接收方收到后停止等待，但**保留已收到的部分**以便将来续传。
    FileAbort { generation: u64 },
    /// 发送方无法提供某文件（已被删除/移动/无权限）。
    FileUnavailable {
        generation: u64,
        file_id: u64,
        reason: String,
    },

    /// 心跳，保持连接与探活。
    Ping,
    Pong,
}

impl SyncMessage {
    /// 序列化为紧凑二进制。
    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// 从二进制反序列化。
    pub fn decode(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ClipContent;

    #[test]
    fn clip_message_roundtrips() {
        let msg = SyncMessage::Clip {
            origin: DeviceId::from_public_key(b"dev"),
            seq: 42,
            content_hash: 0xdead_beef,
            content: ClipContent::Text("hello".into()),
        };
        let bytes = msg.encode().unwrap();
        let back = SyncMessage::decode(&bytes).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn hello_roundtrips() {
        let msg = SyncMessage::Hello {
            protocol: PROTOCOL_VERSION,
            device: DeviceId::from_public_key(b"dev"),
            device_name: "MacBook".into(),
        };
        let bytes = msg.encode().unwrap();
        assert_eq!(SyncMessage::decode(&bytes).unwrap(), msg);
    }

    #[test]
    fn addresses_message_roundtrips() {
        let msg = SyncMessage::Addresses {
            addrs: vec![
                "192.168.1.7:47684".parse().unwrap(),
                "100.101.102.103:47684".parse().unwrap(),
                "[fd7a:115c:a1e0::1]:47684".parse().unwrap(),
            ],
        };
        let bytes = msg.encode().unwrap();
        assert_eq!(SyncMessage::decode(&bytes).unwrap(), msg);
    }
}
