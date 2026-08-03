//! 设备身份。
//!
//! 设备的稳定身份由其 Noise 静态公钥决定；`DeviceId` 是公钥的
//! 短指纹，用于日志、mDNS TXT 记录与设备去重展示。

use serde::{Deserialize, Serialize};

use crate::hash::fnv1a_64;

/// 设备稳定标识：Noise 静态公钥的 64 位指纹的十六进制串。
///
/// 注意：这是展示/匹配用的短 ID，真正的身份认证依赖握手时对
/// 完整静态公钥的校验（见 `clipsync-net`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(String);

impl DeviceId {
    /// 由完整静态公钥派生短指纹 ID。
    pub fn from_public_key(pubkey: &[u8]) -> Self {
        Self(format!("{:016x}", fnv1a_64(pubkey)))
    }

    /// 从已持久化的字符串还原。
    pub fn from_hex(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 已配对设备的展示信息（持久化在配对记录中）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: DeviceId,
    /// 用户可读设备名（如主机名）。
    pub name: String,
}

impl DeviceInfo {
    pub fn new(id: DeviceId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_stable_for_same_key() {
        let k = b"some-static-public-key-bytes";
        assert_eq!(DeviceId::from_public_key(k), DeviceId::from_public_key(k));
    }

    #[test]
    fn id_differs_for_different_keys() {
        assert_ne!(
            DeviceId::from_public_key(b"key-a"),
            DeviceId::from_public_key(b"key-b")
        );
    }

    #[test]
    fn id_roundtrips_through_hex() {
        let id = DeviceId::from_public_key(b"abc");
        let restored = DeviceId::from_hex(id.as_str());
        assert_eq!(id, restored);
    }
}
