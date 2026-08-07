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
const ALPHABET: &[u8] = b"0123456789";

/// 配对码长度（字符数）。
///
/// **为什么是 4 位数字，而不是原来的 6 位字母数字**：这串码**只能靠人念、
/// 人敲**。本工具要解决的就是"跨设备复制粘贴"，在它跑通之前，用剪贴板把码
/// 递过去是循环依赖；也不该假设用户手边有微信之类的传输通道。
///
/// 4 位十进制 = 1 万种组合，配合下面两条硬约束足够：
///   - **一次性 + 60 秒有效**（见 `HOST_SESSION_TIMEOUT`）；
///   - **每个会话最多 3 次真正的猜测**（见 `MAX_FAILED_ATTEMPTS`），
///     SPAKE2 保证每猜一次都要走完一轮握手，离线穷举不可能。
///
/// 于是单个会话被猜中的概率是 3/10000，且攻击者必须在那 60 秒里够得着
/// 配对端口。这个代价换来的是"看一眼、敲四个数字"。
pub const CODE_LEN: usize = 4;

/// 拒绝采样的上界：`b % 10` 会让 0–5 比 6–9 各多出 1/256 的概率
/// （256 不是 10 的整数倍）。原来 30 个字符的字母表下这点偏差无关紧要，
/// 4 位数字只有 1 万种组合，不该在源头上再削掉一点熵。
const REJECT_AT_OR_ABOVE: u8 = 250;

/// 一次性配对码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode(String);

impl PairingCode {
    /// 由随机字节生成配对码。
    ///
    /// `random_bytes` 应显著多于 `CODE_LEN`（拒绝采样会丢掉约 2% 的字节）。
    /// 此处保持无随机依赖，便于纯逻辑测试；[`generate`](Self::generate)
    /// 一次给 16 字节，走不到末尾的补齐分支。
    pub fn from_entropy(random_bytes: &[u8]) -> Self {
        let mut code = String::with_capacity(CODE_LEN);
        for &b in random_bytes {
            if code.len() == CODE_LEN {
                break;
            }
            if b < REJECT_AT_OR_ABOVE {
                code.push(char::from(ALPHABET[(b % 10) as usize]));
            }
        }
        // 给的熵不够时补齐，保证返回值总是一个合法配对码而不是半截串。
        while code.len() < CODE_LEN {
            code.push('0');
        }
        Self(code)
    }

    /// 用密码学安全随机源生成一个新配对码。
    pub fn generate() -> Self {
        use rand::RngCore;
        // 16 字节里凑不出 4 个可用字节的概率约 10^-27，不必循环重取。
        let mut bytes = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self::from_entropy(&bytes)
    }

    /// 从用户输入解析（去空白与连字符；校验字符集与长度）。
    pub fn parse(input: &str) -> Option<Self> {
        let cleaned: String = input
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
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

    /// 这台设备是**怎么进来的**：`None` 表示用户亲手配对，`Some(名字)`
    /// 表示由该设备引荐而来。
    ///
    /// 记下来是为了让用户能分辨——引荐是传递信任，你的设备表里可能出现
    /// 从没亲手加过的设备，界面上不区分就等于把这件事藏起来了。
    /// `serde(default)` 保证旧记录仍可读取（一律视为亲手配对）。
    #[serde(default)]
    pub introduced_by: Option<String>,
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
