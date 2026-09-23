//! 密钥绑定:把"身份"(Ed25519)与"握手静态密钥"(X25519)锁死。
//!
//! # 为什么必须存在这一层
//!
//! Noise IK 握手能认证的只有 **X25519 静态密钥**本身,它并不知道
//! "这个密钥属于哪个 NodeId"。若不做绑定,攻击流程是:
//!
//! ```text
//! 1. 攻击者生成自己的密钥对
//! 2. 伪造发现报文,声称自己是受害者 NodeId,附带自己的 X25519 公钥
//! 3. 受害者的同伴用"攻击者的公钥"发起 IK 握手 → 加密通道建立
//!    —— 攻击者成功冒充了 NodeId,全程不需要伪造任何签名
//! ```
//!
//! 绑定签名堵死这条路:**Ed25519 身份私钥对 X25519 静态公钥签名**。
//! 验证方同时检查签名与 `NodeId == SHA-256(Ed25519 公钥)[0..16]`,
//! 于是"声称的 NodeId ↔ 握手密钥"的对应关系有了密码学证明。
//!
//! # 域分离
//!
//! 签名对象带固定前缀 [`BINDING_CONTEXT`],防止同一把 Ed25519 密钥
//! 在其它场景(比如未来的文件签名)产生的签名被挪用来冒充密钥绑定。

use fq_proto::NodeId;

use crate::error::{Error, Result};
use crate::identity::{Identity, PUBLIC_KEY_LEN, SIGNATURE_LEN, verify_signature};

/// 绑定签名的域分离上下文。
pub const BINDING_CONTEXT: &[u8] = b"feiqiu-r/v1/noise-static-key-binding";

fn binding_message(static_public: &[u8; PUBLIC_KEY_LEN]) -> Vec<u8> {
    let mut message = Vec::with_capacity(BINDING_CONTEXT.len() + PUBLIC_KEY_LEN);
    message.extend_from_slice(BINDING_CONTEXT);
    message.extend_from_slice(static_public);
    message
}

/// 用身份私钥为 Noise 静态公钥生成绑定签名。
pub fn sign_static_key_binding(
    identity: &Identity,
    static_public: &[u8; PUBLIC_KEY_LEN],
) -> [u8; SIGNATURE_LEN] {
    identity.sign(&binding_message(static_public))
}

/// 验证绑定签名;通过则返回该身份对应的 [`NodeId`]。
///
/// 这是收到发现报文后的**必经检查**:
/// 任何"声称的 NodeId 与绑定验证结果不一致"的节点都必须被直接丢弃。
pub fn verify_static_key_binding(
    identity_public_key: &[u8; PUBLIC_KEY_LEN],
    static_public: &[u8; PUBLIC_KEY_LEN],
    signature: &[u8; SIGNATURE_LEN],
) -> Result<NodeId> {
    verify_signature(
        identity_public_key,
        &binding_message(static_public),
        signature,
    )
    .map_err(|e| Error::BindingInvalid(format!("{e}")))?;

    Ok(NodeId::from_public_key(identity_public_key))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn valid_binding_yields_node_id() {
        let identity = Identity::generate().unwrap();
        let static_public = [0xA5u8; PUBLIC_KEY_LEN];
        let signature = sign_static_key_binding(&identity, &static_public);

        let derived =
            verify_static_key_binding(&identity.public_key(), &static_public, &signature).unwrap();
        assert_eq!(derived, identity.node_id());
    }

    #[test]
    fn binding_is_bound_to_specific_static_key() {
        let identity = Identity::generate().unwrap();
        let real_static = [0x01u8; PUBLIC_KEY_LEN];
        let attacker_static = [0x02u8; PUBLIC_KEY_LEN];

        // 用真实静态密钥的签名,去验证攻击者的静态密钥 → 必须失败
        let signature = sign_static_key_binding(&identity, &real_static);
        assert!(
            verify_static_key_binding(&identity.public_key(), &attacker_static, &signature)
                .is_err(),
            "把 A 密钥的绑定签名安到 B 密钥上必须被拒绝"
        );
    }

    #[test]
    fn binding_cannot_be_signed_by_other_identity() {
        let victim = Identity::generate().unwrap();
        let attacker = Identity::generate().unwrap();
        let static_public = [0x03u8; PUBLIC_KEY_LEN];

        // 攻击者给自己的静态密钥签名,冒充受害者身份公钥 → 必须失败
        let signature = sign_static_key_binding(&attacker, &static_public);
        assert!(
            verify_static_key_binding(&victim.public_key(), &static_public, &signature).is_err(),
            "别人的签名不得通过受害者的身份验证"
        );
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut state = 0x1234_5678_9ABC_DEF0u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for _ in 0..1000 {
            let pk: [u8; PUBLIC_KEY_LEN] = std::array::from_fn(|_| next() as u8);
            let sk: [u8; PUBLIC_KEY_LEN] = std::array::from_fn(|_| next() as u8);
            let sig: [u8; SIGNATURE_LEN] = std::array::from_fn(|_| next() as u8);
            let _ = verify_static_key_binding(&pk, &sk, &sig);
        }
    }
}
