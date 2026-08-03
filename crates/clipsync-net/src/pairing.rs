//! 一次性配对码与配对记录。
//!
//! 配对流程（详见 M2 实现）：
//!   1. 设备 A 托盘生成配对码（`PairingCode`）——短、易输入。
//!   2. 设备 B 输入该码，双方用 PAKE（spake2）以此短口令协商强会话密钥，
//!      在不安全信道上抗中间人。
//!   3. 借该会话密钥安全交换各自的 Noise 静态公钥，互存为 `PairingRecord`。
//!   4. 此后连接用 Noise_IK 基于已存公钥互认，永久免再配对。
//!
//! 本模块定义配对码与配对记录的数据类型；PAKE 协商在 M2 引入 spake2 实现。

use clipsync_core::DeviceId;
use serde::{Deserialize, Serialize};

/// 配对码字符集：去除易混淆字符（0/O、1/I/L）以便口头/手动输入。
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/// 配对码长度（字符数）。6 位在此字符集下约 30^6 ≈ 7.3e8 组合，
/// 配合"一次性、短时效、错误锁定"足以抵御在线猜测。
pub const CODE_LEN: usize = 6;

/// 一次性配对码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode(String);

impl PairingCode {
    /// 由随机字节生成配对码。
    ///
    /// `random_bytes` 需至少 `CODE_LEN` 字节，由调用方用密码学安全的
    /// 随机源提供（如 M2 引入的 `rand`/`getrandom`）。此处保持无随机依赖，
    /// 便于纯逻辑测试。
    pub fn from_entropy(random_bytes: &[u8]) -> Self {
        assert!(
            random_bytes.len() >= CODE_LEN,
            "need at least {CODE_LEN} random bytes"
        );
        let code: String = random_bytes[..CODE_LEN]
            .iter()
            .map(|&b| ALPHABET[b as usize % ALPHABET.len()] as char)
            .collect();
        Self(code)
    }

    /// 用密码学安全随机源生成一个新配对码。
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut bytes = [0u8; CODE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self::from_entropy(&bytes)
    }

    /// 从用户输入解析（大写化、去空白；校验字符集与长度）。
    pub fn parse(input: &str) -> Option<Self> {
        let cleaned: String = input
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .flat_map(|c| c.to_uppercase())
            .collect();
        if cleaned.len() != CODE_LEN {
            return None;
        }
        if !cleaned.bytes().all(|b| ALPHABET.contains(&b)) {
            return None;
        }
        Some(Self(cleaned))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 一条已配对设备记录，持久化到配置目录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingRecord {
    pub device: DeviceId,
    pub name: String,
    /// 对端 Noise 静态公钥（长期设备身份，握手时校验）。
    pub static_public_key: Vec<u8>,
    /// 配对时对端宣告的可达地址（物理网卡 / 覆盖网 / 公网，不区分产品）。
    ///
    /// 这是首次连接的地址来源；之后会由局域网信标与加密通道内的地址通告
    /// 持续刷新。`serde(default)` 保证旧版本记录仍可读取。
    #[serde(default)]
    pub addrs: Vec<std::net::SocketAddr>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_code_has_correct_length_and_alphabet() {
        let code = PairingCode::from_entropy(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(code.as_str().len(), CODE_LEN);
        assert!(code.as_str().bytes().all(|b| ALPHABET.contains(&b)));
    }

    #[test]
    fn parse_normalizes_case_and_separators() {
        // 生成一个已知码，再以小写/带分隔符形式解析回来。
        let code = PairingCode::from_entropy(&[0, 1, 2, 3, 4, 5]);
        let s = code.as_str().to_string();
        let lowered = s.to_lowercase();
        let with_dash = format!("{}-{}", &lowered[..3], &lowered[3..]);
        assert_eq!(PairingCode::parse(&with_dash), Some(code));
    }

    #[test]
    fn parse_rejects_wrong_length() {
        assert!(PairingCode::parse("ABC").is_none());
    }

    #[test]
    fn parse_rejects_illegal_chars() {
        // '0' 和 '1' 不在字符集内。
        assert!(PairingCode::parse("000001").is_none());
    }
}
