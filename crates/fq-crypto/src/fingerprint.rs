//! 指纹:静态公钥的人类可读校验串。
//!
//! 用于 TOFU 首次确认与"密钥变更"告警的 UI 展示。域分离后取 SHA-256
//! 前 16 字节,格式化为 8 组 4 位大写 hex:`AB12-CD34-…`。
//! 展示指纹而不是原始公钥,是为了让用户能**肉眼比对**。

use sha2::{Digest, Sha256};

use crate::channel::STATIC_KEY_LEN;

/// 指纹计算的域分离上下文。
pub const FINGERPRINT_CONTEXT: &[u8] = b"feiqiu-r/v1/fingerprint";

/// 计算静态公钥指纹。
pub fn static_key_fingerprint(static_public: &[u8; STATIC_KEY_LEN]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(FINGERPRINT_CONTEXT);
    hasher.update(static_public);
    let digest = hasher.finalize();

    let raw = hex::encode(&digest[..16]).to_uppercase();
    let mut out = String::with_capacity(raw.len() + raw.len() / 4);
    for (i, ch) in raw.chars().enumerate() {
        if i > 0 && i % 4 == 0 {
            out.push('-');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn format_is_eight_groups_of_four() {
        let fp = static_key_fingerprint(&[0u8; STATIC_KEY_LEN]);
        let groups: Vec<&str> = fp.split('-').collect();
        assert_eq!(groups.len(), 8, "应为 8 组: {fp}");
        for group in groups {
            assert_eq!(group.len(), 4, "每组 4 个字符: {fp}");
            assert!(
                group.chars().all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c)),
                "应为大写 hex: {fp}"
            );
        }
    }

    #[test]
    fn deterministic_and_key_sensitive() {
        let a = static_key_fingerprint(&[1u8; STATIC_KEY_LEN]);
        let a_again = static_key_fingerprint(&[1u8; STATIC_KEY_LEN]);
        let b = static_key_fingerprint(&[2u8; STATIC_KEY_LEN]);

        assert_eq!(a, a_again, "同一密钥指纹必须稳定");
        assert_ne!(a, b, "不同密钥指纹必须不同");
    }
}
