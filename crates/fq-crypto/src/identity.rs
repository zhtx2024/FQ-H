//! 节点身份:Ed25519 签名密钥 + 派生的 [`NodeId`]。
//!
//! 身份模型(协议 v1):
//!
//! ```text
//! Ed25519 种子(32 字节,绝对保密)
//!   └─ Ed25519 公钥(32 字节,随发现报文公开)
//!        └─ NodeId = SHA-256(公钥)[0..16]     ← 全网唯一身份
//! ```
//!
//! 为什么身份用 Ed25519、握手用 X25519、两者如何绑定:见 [`crate::binding`]。
//! 简言之:**签名密钥证明"你是谁",握手密钥建立"保密通道",绑定签名把两者锁死**。
//! 这样攻击者拿到自己的密钥对也无法冒充别人的 NodeId 发起握手。

use std::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use fq_proto::NodeId;
use zeroize::Zeroizing;

use crate::error::{Error, Result};

/// Ed25519 公钥长度(字节)。
pub const PUBLIC_KEY_LEN: usize = 32;
/// Ed25519 签名长度(字节)。
pub const SIGNATURE_LEN: usize = 64;

/// 节点身份。
///
/// 持有 Ed25519 签名私钥。`Debug` 输出刻意不含任何秘密材料。
#[derive(Clone)]
pub struct Identity {
    signing: SigningKey,
    node_id: NodeId,
}

impl Identity {
    /// 用系统熵源生成新身份。
    pub fn generate() -> Result<Self> {
        let mut seed = Zeroizing::new([0u8; PUBLIC_KEY_LEN]);
        getrandom::fill(&mut seed[..]).map_err(|e| Error::Entropy(e.to_string()))?;
        Ok(Self::from_seed(&seed))
    }

    /// 由种子确定性构造身份(持久化恢复与测试用)。
    pub fn from_seed(seed: &[u8; PUBLIC_KEY_LEN]) -> Self {
        let signing = SigningKey::from_bytes(seed);
        let public = signing.verifying_key().to_bytes();
        Self {
            node_id: NodeId::from_public_key(&public),
            signing,
        }
    }

    /// 本节点 ID。
    pub fn node_id(&self) -> NodeId {
        self.node_id
    }

    /// Ed25519 公钥(可公开)。
    pub fn public_key(&self) -> [u8; PUBLIC_KEY_LEN] {
        self.signing.verifying_key().to_bytes()
    }

    /// 对消息签名,返回 64 字节签名。
    pub fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_LEN] {
        self.signing.sign(message).to_bytes()
    }

    /// 私钥种子(仅持久化用;调用方必须妥善保管返回值)。
    pub fn seed_bytes(&self) -> Zeroizing<[u8; PUBLIC_KEY_LEN]> {
        Zeroizing::new(self.signing.to_bytes())
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 绝不输出任何秘密材料
        write!(f, "Identity({})", self.node_id)
    }
}

/// 用公钥验证签名。签名不合法、公钥非法都返回 [`Error::Crypto`]。
pub fn verify_signature(
    public_key: &[u8; PUBLIC_KEY_LEN],
    message: &[u8],
    signature: &[u8; SIGNATURE_LEN],
) -> Result<()> {
    let verifying = VerifyingKey::from_bytes(public_key)
        .map_err(|e| Error::Crypto(format!("非法公钥: {e}")))?;
    let signature = Signature::from_bytes(signature);
    verifying
        .verify(message, &signature)
        .map_err(|e| Error::Crypto(format!("签名验证失败: {e}")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_roundtrip() {
        let identity = Identity::generate().unwrap();
        let message = b"hello feiqiu-r";
        let signature = identity.sign(message);

        assert!(verify_signature(&identity.public_key(), message, &signature).is_ok());
    }

    #[test]
    fn wrong_message_or_key_fails() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let signature = a.sign(b"message A");

        // 消息被换
        assert!(verify_signature(&a.public_key(), b"message B", &signature).is_err());
        // 验证方被换
        assert!(verify_signature(&b.public_key(), b"message A", &signature).is_err());
    }

    #[test]
    fn node_id_is_derived_from_public_key() {
        let identity = Identity::generate().unwrap();
        assert_eq!(
            identity.node_id(),
            NodeId::from_public_key(&identity.public_key())
        );
    }

    #[test]
    fn deterministic_from_seed() {
        let seed = [0x42u8; 32];
        let first = Identity::from_seed(&seed);
        let second = Identity::from_seed(&seed);
        assert_eq!(first.node_id(), second.node_id());
        assert_eq!(first.public_key(), second.public_key());
        // 不同种子 → 不同身份
        let other = Identity::from_seed(&[0x43u8; 32]);
        assert_ne!(first.node_id(), other.node_id());
    }

    #[test]
    fn debug_output_contains_no_secret() {
        let identity = Identity::generate().unwrap();
        let seed = hex::encode(identity.seed_bytes().as_slice());
        let debug = format!("{identity:?}");
        assert!(!debug.contains(&seed), "Debug 输出不得泄露种子");
        assert!(debug.contains("Identity"), "Debug 应标明类型");
    }
}
