//! 确定性内容哈希（FNV-1a, 64 位），用于防回环与去重。
//!
//! 选择 FNV-1a 而非 std 的 `DefaultHasher`(SipHash)：零依赖、结果在
//! 跨版本/跨设备间稳定，便于放入网络消息并在对端比较。剪贴板去重
//! 不是安全场景，无需抗碰撞强度。

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 增量哈希器：可分多次 `update`，最后 `finish`。
#[derive(Debug, Clone)]
pub struct Hasher(u64);

impl Hasher {
    #[inline]
    pub fn new() -> Self {
        Self(FNV_OFFSET)
    }

    #[inline]
    pub fn update(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
        self.0 = h;
    }

    #[inline]
    pub fn finish(&self) -> u64 {
        self.0
    }
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

/// 一次性哈希便捷函数。
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h = Hasher::new();
    h.update(bytes);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        // FNV-1a 64 位对空串的标准值即 offset basis。
        assert_eq!(fnv1a_64(b""), FNV_OFFSET);
        // "a" 的标准 FNV-1a 64 值。
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn incremental_equals_oneshot() {
        let mut h = Hasher::new();
        h.update(b"hello ");
        h.update(b"world");
        assert_eq!(h.finish(), fnv1a_64(b"hello world"));
    }

    #[test]
    fn different_inputs_differ() {
        assert_ne!(fnv1a_64(b"foo"), fnv1a_64(b"bar"));
    }
}
