//! 本机长期加密身份：Noise 静态密钥对。
//!
//! 每台设备生成一对 X25519 静态密钥并持久化，公钥即设备的长期身份
//! （`DeviceId` 由其派生）。配对时双方交换静态公钥；此后每次连接用
//! `Noise_IK` 基于已存公钥互认，无需重新配对。
//!
//! Noise 参数选用 `Noise_IK_25519_ChaChaPoly_BLAKE2s`：
//!   - IK：发起方已知响应方静态公钥（配对后成立），响应方在握手中认证发起方，
//!     双向静态密钥认证，抗中间人。
//!   - 25519 / ChaChaPoly / BLAKE2s：纯软件实现，snow 默认 resolver 即可，
//!     无需 C 依赖，契合"稳定简便 + 低占用"。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// ClipSync 使用的 Noise 协议参数字符串。
pub const NOISE_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

/// 本机静态密钥对。私钥仅存于本地配置目录，绝不外传。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticIdentity {
    /// X25519 私钥（32 字节）。
    pub private_key: Vec<u8>,
    /// X25519 公钥（32 字节），对外身份。
    pub public_key: Vec<u8>,
}

impl StaticIdentity {
    /// 生成一对新的静态密钥。
    pub fn generate() -> Result<Self> {
        let builder = snow::Builder::new(NOISE_PARAMS.parse().context("解析 Noise 参数失败")?);
        let keypair = builder
            .generate_keypair()
            .context("生成 Noise 静态密钥失败")?;
        Ok(Self {
            private_key: keypair.private,
            public_key: keypair.public,
        })
    }

    /// 派生本设备 ID（公钥指纹）。
    pub fn device_id(&self) -> clipsync_core::DeviceId {
        clipsync_core::DeviceId::from_public_key(&self.public_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_have_expected_length() {
        let id = StaticIdentity::generate().unwrap();
        assert_eq!(id.private_key.len(), 32);
        assert_eq!(id.public_key.len(), 32);
    }

    #[test]
    fn distinct_generations_differ() {
        let a = StaticIdentity::generate().unwrap();
        let b = StaticIdentity::generate().unwrap();
        assert_ne!(a.private_key, b.private_key);
        assert_ne!(a.public_key, b.public_key);
    }

    #[test]
    fn device_id_derives_from_public_key() {
        let id = StaticIdentity::generate().unwrap();
        assert_eq!(
            id.device_id(),
            clipsync_core::DeviceId::from_public_key(&id.public_key)
        );
    }
}
