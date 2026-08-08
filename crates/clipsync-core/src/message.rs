//! 设备间传输的应用层消息。
//!
//! 传输编解码用 `postcard`（紧凑二进制）。这些消息在 Noise 加密流
//! 内传输（见 `clipsync-net`），因此本身不含认证字段。

use serde::{Deserialize, Serialize};

use crate::content::ClipContent;
use crate::device::DeviceId;

/// 协议版本，握手后校验，避免不兼容版本互联出错。
///
/// **4**：帧层自适应压缩，消息头帧由 4 字节扩到 9 字节。这一版**不向下兼容**，
/// 且不兼容发生在比 `Hello` 更低的一层——旧版本读到新头帧会直接判为"长度非法"
/// 而断开，根本走不到版本协商。所以 v4 与 v3 之间没有"先探测再降级"的余地，
/// 所有设备必须一起升级。这么改是值得的：图片以裸 RGBA 传输，一张 4K 截图
/// 33 MB，压完只剩 6%。
///
/// **3**：新增 [`SyncMessage::Removed`]（把某台移出设备组）。
/// **2**：新增 [`SyncMessage::Peers`]（设备互相介绍）。旧版本的枚举里没有这个
/// 变体，收到会解码失败并断开重连，陷入死循环——所以发它之前必须先确认对端
/// 版本。确认手段就是 `Hello`：它在 v1 就已定义，旧版本能正常解码后忽略，
/// 因此对老对端发 `Hello` 是安全的；而老对端不会回 `Hello`，我们据此判定
/// "对方是旧版"，从而不发 `Peers`。
pub const PROTOCOL_VERSION: u16 = 4;

/// 把一台设备介绍给另一台所需的全部信息。
///
/// 公钥是认证依据，没有它对方仍然会拒绝连接；地址是首次连接的线索。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerIntro {
    pub device: DeviceId,
    pub name: String,
    pub static_public_key: Vec<u8>,
    pub addrs: Vec<std::net::SocketAddr>,
}

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
    Addresses {
        addrs: Vec<std::net::SocketAddr>,
    },

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
    ///
    /// `offset` 始终指**原始文件**中的位置，与是否压缩无关——这保证了
    /// 断点续传与分段并行传输在开启压缩后仍然成立。
    FileChunk {
        generation: u64,
        file_id: u64,
        offset: u64,
        data: Vec<u8>,
        /// `data` 是否为压缩后的字节。接收方据此决定是否解压。
        /// 旧版本消息没有此字段，默认按未压缩处理。
        #[serde(default)]
        compressed: bool,
        /// 该块解压后的字节数（用于预分配与校验）。压缩时必填。
        #[serde(default)]
        plain_len: u32,
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
    FileAbort {
        generation: u64,
    },
    /// 发送方无法提供某文件（已被删除/移动/无权限）。
    FileUnavailable {
        generation: u64,
        file_id: u64,
        reason: String,
    },

    /// 心跳，保持连接与探活。
    Ping,
    Pong,

    // ———————————————————————————————————————————————————————————
    // 新变体一律追加在末尾：postcard 按变体**索引**编码，插在中间会让
    // 已有变体的编码整体错位，与旧版本彻底不兼容。
    // ———————————————————————————————————————————————————————————
    /// 把本机已知的其它设备介绍给对端。
    ///
    /// **解决的问题**：配对是两两的。A 分别与 B、C 配对后，B 和 C 互不认识
    /// ——它们没交换过公钥，彼此的连接会被"对端未配对"拒绝，信标也互相忽略。
    /// 而用户看到的是两台设备各自"已连接 1/1 台"，一切正常的样子，直到某天
    /// 发现 B 复制的东西在 C 上粘不出来。
    ///
    /// 于是让已连接的双方互相引荐：A 告诉 B"我还认识 C，这是它的公钥和地址"。
    /// B 据此把 C 记为已配对，此后 B 与 C 可直连——**A 关机也不影响**。
    ///
    /// 只在对端协议版本 ≥ 2 时发送（见 [`PROTOCOL_VERSION`]）。
    Peers {
        peers: Vec<PeerIntro>,
    },

    /// 把某台设备**移出设备组**，收到的一方应当忘掉它。
    ///
    /// 引荐让若干设备构成了一个组（A 认识 B、B 认识 C，最终全互联）。既然
    /// 数据模型是组，退出也该是组语义——否则就会出现"我在这台上解除了它，
    /// 别的成员又把它引荐回来"，只能靠一份看不见的拒绝名单去堵，而那份名单
    /// 本身又成了新的困惑来源。
    ///
    /// 一条消息表达两件事，区别只在 `device` 是谁：
    ///   - **别人** → "把它踢出组"，收到的一方删除该设备；
    ///   - **发送者自己** → "我退出了"，收到的一方把发送者删掉。
    ///
    /// 组内任何成员都可以踢任何人——这些本就是同一个人的设备，不必设管理员。
    ///
    /// 只在对端协议版本 ≥ 3 时发送（见 [`PROTOCOL_VERSION`]）。
    Removed {
        device: DeviceId,
    },
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

    #[test]
    fn removed_message_roundtrips() {
        let msg = SyncMessage::Removed {
            device: DeviceId::from_public_key(b"gone"),
        };
        let bytes = msg.encode().unwrap();
        assert_eq!(SyncMessage::decode(&bytes).unwrap(), msg);
    }

    /// `Removed` 必须排在枚举**末尾**。
    ///
    /// postcard 按变体下标编码，往中间插一个变体会让所有后续变体的下标整体
    /// 后移——旧版本收到新版本的消息会解码成完全不相干的类型，而且不报错。
    /// 这个断言把"新变体一律追加在最后"这条纪律钉死在测试里。
    #[test]
    fn removed_is_the_last_variant() {
        let last = SyncMessage::Removed {
            device: DeviceId::from_public_key(b"x"),
        };
        let idx = last.encode().unwrap()[0];
        // 逐个编码其余变体，确认没有谁的下标比它还大。
        let others = [
            SyncMessage::Hello {
                protocol: PROTOCOL_VERSION,
                device: DeviceId::from_public_key(b"x"),
                device_name: String::new(),
            },
            SyncMessage::Addresses { addrs: vec![] },
            SyncMessage::Peers { peers: vec![] },
        ];
        for m in others {
            assert!(
                m.encode().unwrap()[0] < idx,
                "新变体必须追加在枚举末尾，否则会错开旧版本的变体下标"
            );
        }
    }
}
