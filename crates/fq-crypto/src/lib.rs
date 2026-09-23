//! # fq-crypto
//!
//! feiqiu-r 的身份与安全通道层,回答三个问题:
//!
//! 1. **你是谁** —— [`Identity`]:Ed25519 身份密钥,派生全网唯一的 [`fq_proto::NodeId`]
//! 2. **通道保密** —— [`channel`]:Noise `IK`(Curve25519 / ChaChaPoly / BLAKE2s)1-RTT 加密通道
//! 3. **凭什么信** —— [`binding`] + [`tofu`]:密钥绑定签名防 NodeId 冒充,TOFU 固定防中间人
//!
//! ## 信任模型(必须按顺序执行)
//!
//! ```text
//! 收到发现报文(NodeId, Ed25519 公钥, X25519 静态公钥, 绑定签名)
//!   │
//!   ├─ ① binding::verify_static_key_binding   ← 证明"X25519 密钥确实属于该 NodeId"
//!   │
//!   ├─ ② tofu::TofuStore::verify              ← FirstUse / Trusted / Changed
//!   │       └─ Changed 必须交用户裁决,绝不静默换绑
//!   │
//!   └─ ③ channel::HandshakeInitiator::start   ← 用已验证的静态公钥发起 IK 握手
//! ```
//!
//! 跳过 ①② 直接握手的实现等于没有认证 —— 那是本项目明确禁止的。
//!
//! ## 快速上手(完整握手示例见 `tests/secure_channel.rs`)
//!
//! ```
//! use fq_crypto::{HandshakeInitiator, HandshakeResponder, StaticKeys};
//!
//! let alice_static = StaticKeys::generate().unwrap();
//! let bob_static = StaticKeys::generate().unwrap();
//!
//! // Alice 已(通过发现报文 + 绑定验证)知道 Bob 的静态公钥
//! let mut alice = HandshakeInitiator::start(&alice_static, &bob_static.public()).unwrap();
//! let msg1 = alice.first_message(b"").unwrap();
//!
//! let (msg2, _bob_channel) = HandshakeResponder::listen(&bob_static)
//!     .unwrap()
//!     .respond(&msg1)
//!     .unwrap();
//! let mut alice_channel = alice.finish(&msg2).unwrap();
//!
//! let ciphertext = alice_channel.encrypt("你好".as_bytes()).unwrap();
//! // …把 ciphertext 发给 Bob…
//! ```
//!
//! ## 安全清单
//!
//! * 私钥材料一律 `Zeroizing` 包裹,`Debug` 输出不含任何秘密
//! * 解密/验证路径对不可信输入只返回 [`Error`],不 panic(有模糊测试)
//! * 身份文件原子写(临时文件 + rename),损坏文件被拒绝而非 panic

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod binding;
pub mod channel;
pub mod error;
pub mod fingerprint;
pub mod identity;
pub mod persist;
pub mod tofu;

pub use binding::{BINDING_CONTEXT, sign_static_key_binding, verify_static_key_binding};
pub use channel::{
    HandshakeInitiator, HandshakeResponder, SecureChannel, StaticKeys, NOISE_PATTERN, STATIC_KEY_LEN,
};
pub use error::{Error, Result};
pub use fingerprint::{FINGERPRINT_CONTEXT, static_key_fingerprint};
pub use identity::{Identity, PUBLIC_KEY_LEN, SIGNATURE_LEN, verify_signature};
pub use persist::{
    load_identity, load_or_create_identity, load_or_create_static_keys, load_static_keys, load_tofu,
    save_identity, save_static_keys, save_tofu,
};
pub use tofu::{TrustDecision, TofuStore};
